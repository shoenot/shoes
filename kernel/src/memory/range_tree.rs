use alloc::boxed::Box;

type Link<T> = Option<Box<Node<T>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Color {
    Red,
    Black,
}

#[derive(Debug, Clone)]
struct Node<T> {
    start: usize,
    end: usize,
    value: T,
    color: Color,
    left: Link<T>,
    right: Link<T>,
}

impl<T> Node<T> {
    fn new(start: usize, end: usize, value: T) -> Box<Self> {
        Box::new(Self {
            start,
            end,
            value,
            color: Color::Red,
            left: None,
            right: None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeMapError {
    Empty, 
    Overflow,
    Overlap,
    NotFound,
    Mismatch,
    InvalidAlignment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeEntry<'a, T> {
    pub start: usize,
    pub end: usize,
    pub value: &'a T,
}

impl<'a, T> RangeEntry<'a, T> {
    pub fn size(&self) -> usize {
        self.end - self.start
    }
}

#[derive(Debug, Clone)]
pub struct RangeMap<T> {
    root: Link<T>,
    len: usize,
}

impl<T> RangeMap<T> {
    pub const fn new() -> Self {
        Self { root: None, len: 0 }
    }

    pub fn len(&self) -> usize { self.len }

    pub fn is_empty(&self) -> bool { self.len == 0 }

    pub fn insert(&mut self, start: usize, end: usize, value: T) -> Result<(), RangeMapError> {
        if start >= end { return Err(RangeMapError::Empty); }
        if self.find_overlap(start, end).is_some() { return Err(RangeMapError::Overlap) }

        let root = self.root.take();
        self.root = Some(insert_node(root, start, end, value));
        if let Some(root) = self.root.as_mut() {
            root.color = Color::Black;
        }

        self.len += 1;
        Ok(())
    }

    pub fn remove(&mut self, start: usize) -> Option<T> {
        self.get_by_start(start)?;
        if let Some(root) = self.root.as_mut() {
            if !is_red(&root.left) && !is_red(&root.right) {
                root.color = Color::Red;
            }
        }

        let mut removed = None;
        let root = self.root.take().expect("range tree root missing during remove");
        self.root = delete_node(root, start, &mut removed);

        if let Some(root) = self.root.as_mut() {
            root.color = Color::Black;
        }

        if removed.is_some() { self.len -= 1; }
        removed
    }

    pub fn get(&self, addr: usize) -> Option<RangeEntry<'_, T>> {
        let mut current = &self.root;
        while let Some(node) = current {
            if addr < node.start {
                current = &node.left;
            } else if addr >= node.end {
                current = &node.right;
            } else {
                return Some(RangeEntry { start:node.start, end: node.end, value: &node.value })
            }
        }
        None
    }

    pub fn get_mut(&mut self, addr: usize) -> Option<(usize, usize, &mut T)> {
        let mut current = &mut self.root;
        while let Some(node) = current {
            if addr < node.start {
                current = &mut node.left;
            } else if addr >= node.end {
                current = &mut node.right;
            } else {
                return Some((node.start, node.end, &mut node.value ))
            }
        }
        None
    }

    pub fn get_by_start(&self, start: usize) -> Option<RangeEntry<'_, T>> {
        let mut current = &self.root;
        while let Some(node) = current {
            if start < node.start {
                current = &node.left;
            } else if start > node.start {
                current = &node.right;
            } else {
                return Some(RangeEntry { start: node.start, end: node.end, value: &node.value })
            }
        }
        None
    }

    pub fn find_overlap(&self, start: usize, end: usize) -> Option<RangeEntry<'_, T>> {
        if start >= end { return None; }

        let mut current = &self.root;
        while let Some(node) = current {
            if end <= node.start {
                current = &node.left;
            } else if start >= node.end {
                current = &node.right;
            } else {
                return Some(RangeEntry { start: node.start, end: node.end, value: &node.value })
            }
        }
        None
    }

    pub fn for_each(&self, mut f: impl FnMut(RangeEntry<'_, T>)) {
        visit_in_order(&self.root, &mut f);
    }

    pub fn find_gap(&self, size: usize, align: usize, min: usize, max: usize) -> Result<Option<usize>, RangeMapError> {
        if align == 0 { return Err(RangeMapError::InvalidAlignment); }
        if size == 0 || min >= max { return Ok(None); }

        let mut cursor = min;
        let mut found = None;
        
        self.for_each(|entry| {
            if found.is_some() { return; }
            if entry.end <= min { return; }

            if entry.start >= max {
                let candidate = align_up(cursor, align);
                if let Some(candidate) = candidate {
                    if candidate.checked_add(size).is_some_and(|end| end <= max) {
                        found = Some(candidate);
                    }
                }
                return;
            }

            let gap_end = entry.start.min(max);
            let candidate = align_up(cursor, align);

            if let Some(candidate) = candidate {
                if candidate.checked_add(size).is_some_and(|end| end <= gap_end) {
                    found = Some(candidate);
                    return;
                }
            }


            if entry.end > cursor {
                cursor = entry.end;
            }
        });

        if found.is_some() {
            return Ok(found);
        }

        let Some(candidate) = align_up(cursor, align) else { return Ok(None); };
        if candidate.checked_add(size).is_some_and(|end| end <= max) {
            Ok(Some(candidate))
        } else {
            Ok(None)
        }
    }

    pub fn validate(&self) -> bool {
        let Some(root) = &self.root else {
            return self.len == 0;
        };

        if root.color != Color::Black {
            return false;
        }

        let mut count = 0;
        let mut previous_end = None;
        let ordered = validate_order(&self.root, &mut previous_end, &mut count);
        let balanced = black_height(&self.root).is_some();
        ordered && balanced && count == self.len
    }

    pub fn insert_size(&mut self, start: usize, size: usize, value: T) -> Result<(), RangeMapError> {
        if size == 0 { return Err(RangeMapError::Empty); }
        let end = start.checked_add(size).ok_or(RangeMapError::Overflow)?;
        self.insert(start, end, value)
    }

    pub fn get_by_start_mut(&mut self, start: usize) -> Option<(usize, usize, &mut T)> {
        let mut current = &mut self.root;

        while let Some(node) = current {
            if start < node.start {
                current = &mut node.left;
            } else if start > node.start {
                current = &mut node.right;
            } else {
                return Some((node.start, node.end, &mut node.value))
            }
        }
        None
    }

    pub fn remove_exact(&mut self, start: usize, end: usize) -> Result<T, RangeMapError> {
        let entry = self.get_by_start(start).ok_or(RangeMapError::NotFound)?;
        if entry.end != end { return Err(RangeMapError::Mismatch); }
        self.remove(start).ok_or(RangeMapError::NotFound)
    }
}

fn visit_in_order<'a, T>(node: &'a Link<T>, f: &mut impl FnMut(RangeEntry<'a, T>)) {
    if let Some(node) = node {
        visit_in_order(&node.left, f);

        f(RangeEntry {
            start: node.start,
            end: node.end,
            value: &node.value,
        });

        visit_in_order(&node.right, f);
    }
}

fn align_up(addr: usize, align: usize) -> Option<usize> {
    debug_assert!(align != 0);
    if align == 1 {
        return Some(addr);
    }

    let rem = addr % align;
    if rem == 0 {
        Some(addr)
    } else {
        addr.checked_add(align - rem)
    }
}

impl<T> Default for RangeMap<T> {
    fn default() -> Self { Self::new() }
}

fn is_red<T>(node: &Link<T>) -> bool {
    matches!(node, Some(node) if node.color == Color::Red)
}

fn rotate_left<T>(mut h: Box<Node<T>>) -> Box<Node<T>> {
    let mut x = h.right.take().expect("tried to rotate left without right child");
    h.right = x.left.take();
    x.left = Some(h);
    x.color = x.left.as_ref().expect("left rotation lost the left child").color;
    x.left.as_mut().expect("left rotation lost the left child").color = Color::Red;
    x
}

fn rotate_right<T>(mut h: Box<Node<T>>) -> Box<Node<T>> {
    let mut x = h.left.take().expect("tried to rotate right without left child");
    h.left = x.right.take();
    x.right = Some(h);
    x.color = x.right.as_ref().expect("right rotation lost the right child").color;
    x.right.as_mut().expect("right rotation lost the right child").color = Color::Red;
    x
}

fn flip_colors<T>(h: &mut Box<Node<T>>) {
    h.color = match h.color {
        Color::Red => Color::Black,
        Color::Black => Color::Red,
    };

    if let Some(left) = h.left.as_mut() {
        left.color = match left.color {
            Color::Red => Color::Black,
            Color::Black => Color::Red,
        };
    }

    if let Some(right) = h.right.as_mut() {
        right.color = match right.color {
            Color::Red => Color::Black,
            Color::Black => Color::Red,
        };
    }
}

fn fix_up<T>(mut h: Box<Node<T>>) -> Box<Node<T>> {
    if is_red(&h.right) && !is_red(&h.left) {
        h = rotate_left(h);
    }

    if is_red(&h.left) && is_red(&h.left.as_ref().expect("left child missing").left) {
        h = rotate_right(h);
    }

    if is_red(&h.left) && is_red(&h.right) {
        flip_colors(&mut h);
    }
    h
}

fn move_red_left<T>(mut h: Box<Node<T>>) -> Box<Node<T>> {
    flip_colors(&mut h);
    let right_left_red = h.right.as_ref().is_some_and(|right| is_red(&right.left));
    if right_left_red {
        let right = h.right.take().expect("move_red_left lost the right child");
        h.right = Some(rotate_right(right));
        h = rotate_left(h);
        flip_colors(&mut h);
    }
    h
}

fn move_red_right<T>(mut h: Box<Node<T>>) -> Box<Node<T>> {
    flip_colors(&mut h);
    let left_left_red = h.left.as_ref().is_some_and(|left| is_red(&left.left));
    if left_left_red {
        h = rotate_right(h);
        flip_colors(&mut h);
    }
    h
}

fn insert_node<T>(h: Link<T>, start: usize, end: usize, value: T) -> Box<Node<T>> {
    let Some(mut h) = h else {
        return Node::new(start, end, value);
    };

    if start < h.start {
        h.left = Some(insert_node(h.left.take(), start, end, value));
    } else {
        h.right = Some(insert_node(h.right.take(), start, end, value));
    }

    fix_up(h)
}

fn delete_min<T>(mut h: Box<Node<T>>) -> (Link<T>, Box<Node<T>>) {
    if h.left.is_none() { return (None, h); }
    
    if !is_red(&h.left) && !is_red(&h.left.as_ref().expect("left child is missing").left) {
        h = move_red_left(h);
    }

    let left = h.left.take().expect("delete_min lost left child");
    let (new_left, min) = delete_min(left);
    h.left = new_left;
    (Some(fix_up(h)), min)
}

fn delete_node<T>(mut h: Box<Node<T>>, start: usize, removed: &mut Option<T>) -> Link<T> {
    if start < h.start {
        if h.left.is_some() {
            if !is_red(&h.left) && !is_red(&h.left.as_ref().expect("left child is missing").left) {
                h = move_red_left(h);
            }
            let left = h.left.take().expect("delete_node lost left child");
            h.left = delete_node(left, start, removed);
        }
    } else {
        if is_red(&h.left) { h = rotate_right(h); }

        if start == h.start && h.right.is_none() {
            let Node { value, .. } = *h;
            *removed = Some(value);
            return None;
        }

        if h.right.is_some() {
            if !is_red(&h.right) && !is_red(&h.right.as_ref().expect("right child disappeared").left) {
                h = move_red_right(h);
            }

            if start == h.start {
                let right = h.right.take().expect("delete_node lost right child");
                let (new_right, mut successor) = delete_min(right);
                let Node { value, left, color, .. } = *h;
                *removed = Some(value);

                successor.left = left;
                successor.right = new_right;
                successor.color = color;

                return Some(fix_up(successor));
            }

            let right = h.right.take().expect("delete_node lost right child");
            h.right = delete_node(right, start, removed);
        }
    }
    Some(fix_up(h))
}

fn validate_order<T>(node: &Link<T>, previous_end: &mut Option<usize>, count: &mut usize) -> bool {
    let Some(node) = node else { return true; };
    if !validate_order(&node.left, previous_end, count) { return false; }

    if let Some(previous_end) = previous_end {
        if *previous_end > node.start { return false; }
    }

    if node.start >= node.end { return false; }

    *previous_end = Some(node.end);
    *count += 1;
    validate_order(&node.right, previous_end, count)
}

fn black_height<T>(node: &Link<T>) -> Option<usize> {
    let Some(node) = node else { return Some(1); };

    if node.color == Color::Red && (is_red(&node.left) || is_red(&node.right)) { return None; }

    let left = black_height(&node.left)?;
    let right = black_height(&node.right)?;
    if left != right { return None; }

    Some(left + usize::from(node.color == Color::Black))
}
