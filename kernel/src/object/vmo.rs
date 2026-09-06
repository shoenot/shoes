use alloc::boxed::Box;
use alloc::sync::Arc;
use hal::mmu::PageSize;
use core::any::Any;

use async_trait::async_trait;
use vespertine_abi::op::VmoOp;
use vespertine_abi::{
    AccessRights,
    Invocation,
};

use crate::memory::vmm::{CachePolicy, MapBehavior, VmOptions, VmPermissions, VmaBacking, VmaChargeKind};
use crate::object::invoke::InvocationError;
use crate::object::obj::KernelObject;
use crate::process::current_process;
use crate::memory::vmo::PagedBackingStore;

#[derive(Debug)]
pub struct VmoObject {
    pub vmo: Arc<dyn PagedBackingStore>,
}

impl VmoObject {
    pub fn new(vmo: Arc<dyn PagedBackingStore>) -> Self { Self { vmo } }
}

#[async_trait]
impl KernelObject for VmoObject {
    async fn invoke(&self, invocation: Invocation, _calling_rights: AccessRights) -> Result<usize, InvocationError> {
        if let Invocation::Vmo(vmo_op) = invocation {
            match vmo_op {
                VmoOp::GetPage { offset } => self.vmo.request_page(offset).map_err(|_| InvocationError::InvalidArgument),
                VmoOp::Resize { new_size } => {
                    self.vmo.resize_object(new_size).map_err(|_| InvocationError::UnsupportedOperation)?;
                    Ok(0)
                }
                VmoOp::Clone { offset, len } => {
                    let child_vmo = self.vmo.clone_range(offset, len).map_err(|_| InvocationError::InvalidArgument)?;

                    let child_obj = Arc::new(VmoObject { vmo: child_vmo });

                    let current_proc = current_process().ok_or(InvocationError::UnsupportedOperation)?;

                    let handle_id = current_proc.handles.write().insert(child_obj, AccessRights::all());

                    Ok(handle_id.0 as usize)
                }
                VmoOp::MapIntoProc { vaddr, len, vm_flags } => {
                    let current_proc = current_process().ok_or(InvocationError::UnsupportedOperation)?;
                    let mut vmm = current_proc.vmm.write();

                    let mut perms = VmPermissions::USER;
                    if vm_flags & 1 != 0 { perms = perms | VmPermissions::WRITE; }
                    if vm_flags & 2 != 0 { perms = perms | VmPermissions::EXECUTE; }

                    let opts = VmOptions {
                        permissions: perms,
                        cache: CachePolicy::Normal,
                        page_size: PageSize::Size4K,
                        charge: VmaChargeKind::Private,
                    };

                    let backing = VmaBacking::Vmo(self.vmo.clone());

                    let mapped_addr = if vaddr == 0 {
                        vmm.reserve(len, opts, backing).ok()
                    } else {
                        vmm.map_at(vaddr, len, opts, backing, 0, MapBehavior::ReplaceContained).ok()
                    };

                    mapped_addr.ok_or(InvocationError::OutOfMemory)
                }
            }
        } else {
            Err(InvocationError::UnsupportedOperation)
        }
    }

    fn type_name(&self) -> &'static str { "VMO" }

    fn as_any(&self) -> &dyn Any { self }
}
