use vespertine_abi::op::{
    MemManOp,
    MemPoolOp,
};
use vespertine_abi::{
    AccessRights, FileOp, HandleID, Invocation
};

use crate::object::ipc::socket::SocketEndpoint;
use crate::object::obj::KernelObject;
use crate::object::vfs::{
    kernel_invoke,
    kernel_walk,
};
use crate::klogln;

pub async fn run_pool_tests() {
    let mm_handle = kernel_walk( "/System/Services/MemoryManager", HandleID(0), AccessRights::CREATE).await.expect("No Memory Manager found");

    let root_pool_handle = HandleID(
        kernel_invoke(mm_handle, Invocation::MemoryManager(MemManOp::CreatePool { limit: 0 })).await.expect("Failed to create root pool"),
    );
    klogln!("  - Created global root pool: {:?}", root_pool_handle);

    let sub_pool_handle = HandleID(
        kernel_invoke(root_pool_handle, Invocation::MemPool(MemPoolOp::CreateSubPool { limit: 1024 * 1024 }))
            .await
            .expect("Failed to create sub pool"),
    );
    klogln!("  - Created 1mb sub pool: {:?}", sub_pool_handle);

    let vmo_handle = HandleID(
        kernel_invoke(sub_pool_handle, Invocation::MemPool(MemPoolOp::AllocateVmo { size: 4096 })).await.expect("Failed to allocate VMO"),
    );
    klogln!("  - Allocated 4kb vmo: {:?}", vmo_handle);

    let break_attempt = kernel_invoke(sub_pool_handle, Invocation::MemPool(MemPoolOp::AllocateVmo { size: 1024 * 2048 })).await;
    klogln!("  - Attempted overflow allocation result: {:?}", break_attempt);
}

pub async fn run_socket_ipc_tests() {
    klogln!("  - Initializing kernel socket pair...");
    let (left, right) = SocketEndpoint::new_pair();

    let input = b"Hello Kernel";
    let write_op = FileOp::Write {
        offset: 0,
        buffer_ptr: input.as_ptr() as usize,
        len: input.len(),
    };

    let bytes_written = left.invoke(Invocation::File(write_op), AccessRights::WRITE).await.expect("Failed to write to socket");

    assert_eq!(bytes_written, input.len());

    let mut output = [0u8; 64];
    let read_op = FileOp::Read {
        offset: 0,
        buffer_ptr: output.as_mut_ptr() as usize,
        len: output.len(),
    };

    let bytes_read = right.invoke(Invocation::File(read_op), AccessRights::READ).await.expect("Failed to read from socket");

    assert_eq!(bytes_read, input.len());
    assert_eq!(&output[..bytes_read], input);

    klogln!("  - Socket loopback write/read verified successfully!");
}

pub async fn run_object_tests() {
    klogln!("Running Post-VFS Object and Memory Manager Tests...");
    run_pool_tests().await;

    klogln!("Running Post-VFS Kernel IPC Tests...");
    run_socket_ipc_tests().await;

    klogln!("All Post-VFS tests passed!");
}
