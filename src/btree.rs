use crate::btree::DeletionResult::{PartialInternal, PartialLeaf, Subtree};
use crate::btree::RangeIterState::{InitialState, Internal, LeafLeft, LeafRight};
use crate::page_manager::{Page, PageImpl, PageMut, PageNumber, TransactionalMemory};
use crate::types::{RedbKey, RedbValue};
use std::cmp::{max, Ordering};
use std::convert::TryInto;
use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};

const BTREE_ORDER: usize = 3;

pub struct AccessGuardMut<'a> {
    page: PageMut<'a>,
    offset: usize,
    len: usize,
}

impl<'a> AccessGuardMut<'a> {
    fn new(page: PageMut<'a>, offset: usize, len: usize) -> Self {
        AccessGuardMut { page, offset, len }
    }
}

impl<'a> AsMut<[u8]> for AccessGuardMut<'a> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.page.memory_mut()[self.offset..(self.offset + self.len)]
    }
}

const LEAF: u8 = 1;
const INTERNAL: u8 = 2;

#[derive(Debug)]
enum RangeIterState<'a> {
    InitialState(PageImpl<'a>, bool),
    LeafLeft {
        page: PageImpl<'a>,
        parent: Option<Box<RangeIterState<'a>>>,
        reversed: bool,
    },
    LeafRight {
        page: PageImpl<'a>,
        parent: Option<Box<RangeIterState<'a>>>,
        reversed: bool,
    },
    Internal {
        page: PageImpl<'a>,
        child: usize,
        parent: Option<Box<RangeIterState<'a>>>,
        reversed: bool,
    },
}

impl<'a> RangeIterState<'a> {
    fn forward_next(self, manager: &'a TransactionalMemory) -> Option<RangeIterState> {
        match self {
            RangeIterState::InitialState(root_page, ..) => match root_page.memory()[0] {
                LEAF => Some(LeafLeft {
                    page: root_page,
                    parent: None,
                    reversed: false,
                }),
                INTERNAL => Some(Internal {
                    page: root_page,
                    child: 0,
                    parent: None,
                    reversed: false,
                }),
                _ => unreachable!(),
            },
            RangeIterState::LeafLeft { page, parent, .. } => Some(LeafRight {
                page,
                parent,
                reversed: false,
            }),
            RangeIterState::LeafRight { parent, .. } => parent.map(|x| *x),
            RangeIterState::Internal {
                page,
                child,
                mut parent,
                ..
            } => {
                let accessor = InternalAccessor::new(&page);
                let child_page = accessor.child_page(child).unwrap();
                let child_page = manager.get_page(child_page);
                if child < BTREE_ORDER - 1 && accessor.child_page(child + 1).is_some() {
                    parent = Some(Box::new(Internal {
                        page,
                        child: child + 1,
                        parent,
                        reversed: false,
                    }));
                }
                match child_page.memory()[0] {
                    LEAF => Some(LeafLeft {
                        page: child_page,
                        parent,
                        reversed: false,
                    }),
                    INTERNAL => Some(Internal {
                        page: child_page,
                        child: 0,
                        parent,
                        reversed: false,
                    }),
                    _ => unreachable!(),
                }
            }
        }
    }

    fn backward_next(self, manager: &'a TransactionalMemory) -> Option<RangeIterState> {
        match self {
            RangeIterState::InitialState(root_page, ..) => match root_page.memory()[0] {
                LEAF => Some(LeafRight {
                    page: root_page,
                    parent: None,
                    reversed: true,
                }),
                INTERNAL => {
                    let accessor = InternalAccessor::new(&root_page);
                    let mut index = 0;
                    for i in (0..BTREE_ORDER).rev() {
                        if accessor.child_page(i).is_some() {
                            index = i;
                            break;
                        }
                    }
                    assert!(index > 0);
                    Some(Internal {
                        page: root_page,
                        child: index,
                        parent: None,
                        reversed: true,
                    })
                }
                _ => unreachable!(),
            },
            RangeIterState::LeafLeft { parent, .. } => parent.map(|x| *x),
            RangeIterState::LeafRight { page, parent, .. } => Some(LeafLeft {
                page,
                parent,
                reversed: true,
            }),
            RangeIterState::Internal {
                page,
                child,
                mut parent,
                ..
            } => {
                let child_page = InternalAccessor::new(&page).child_page(child).unwrap();
                let child_page = manager.get_page(child_page);
                if child > 0 {
                    parent = Some(Box::new(Internal {
                        page,
                        child: child - 1,
                        parent,
                        reversed: true,
                    }));
                }
                match child_page.memory()[0] {
                    LEAF => Some(LeafRight {
                        page: child_page,
                        parent,
                        reversed: true,
                    }),
                    INTERNAL => {
                        let accessor = InternalAccessor::new(&child_page);
                        let mut index = 0;
                        for i in (0..BTREE_ORDER).rev() {
                            if accessor.child_page(i).is_some() {
                                index = i;
                                break;
                            }
                        }
                        assert!(index > 0);
                        Some(Internal {
                            page: child_page,
                            child: index,
                            parent,
                            reversed: true,
                        })
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    fn next(self, manager: &'a TransactionalMemory) -> Option<RangeIterState> {
        match &self {
            InitialState(_, reversed) => {
                if *reversed {
                    self.backward_next(manager)
                } else {
                    self.forward_next(manager)
                }
            }
            RangeIterState::LeafLeft { reversed, .. } => {
                if *reversed {
                    self.backward_next(manager)
                } else {
                    self.forward_next(manager)
                }
            }
            RangeIterState::LeafRight { reversed, .. } => {
                if *reversed {
                    self.backward_next(manager)
                } else {
                    self.forward_next(manager)
                }
            }
            RangeIterState::Internal { reversed, .. } => {
                if *reversed {
                    self.backward_next(manager)
                } else {
                    self.forward_next(manager)
                }
            }
        }
    }

    fn get_entry(&self) -> Option<EntryAccessor> {
        match self {
            RangeIterState::LeafLeft { page, .. } => Some(LeafAccessor::new(page).lesser()),
            RangeIterState::LeafRight { page, .. } => LeafAccessor::new(page).greater(),
            _ => None,
        }
    }
}

pub struct BtreeRangeIter<
    'a,
    T: RangeBounds<&'a K>,
    K: RedbKey + ?Sized + 'a,
    V: RedbValue + ?Sized + 'a,
> {
    last: Option<RangeIterState<'a>>,
    table_id: u64,
    query_range: T,
    reversed: bool,
    manager: &'a TransactionalMemory,
    _key_type: PhantomData<K>,
    _value_type: PhantomData<V>,
}

impl<'a, T: RangeBounds<&'a K>, K: RedbKey + ?Sized + 'a, V: RedbValue + ?Sized + 'a>
    BtreeRangeIter<'a, T, K, V>
{
    pub(in crate) fn new(
        root_page: Option<PageImpl<'a>>,
        table_id: u64,
        query_range: T,
        manager: &'a TransactionalMemory,
    ) -> Self {
        Self {
            last: root_page.map(|p| InitialState(p, false)),
            table_id,
            query_range,
            reversed: false,
            manager,
            _key_type: Default::default(),
            _value_type: Default::default(),
        }
    }

    pub(in crate) fn new_reversed(
        root_page: Option<PageImpl<'a>>,
        table_id: u64,
        query_range: T,
        manager: &'a TransactionalMemory,
    ) -> Self {
        Self {
            last: root_page.map(|p| InitialState(p, true)),
            table_id,
            query_range,
            reversed: true,
            manager,
            _key_type: Default::default(),
            _value_type: Default::default(),
        }
    }

    // TODO: we need generic-associated-types to implement Iterator
    pub fn next(&mut self) -> Option<EntryAccessor> {
        if let Some(mut state) = self.last.take() {
            loop {
                if let Some(new_state) = state.next(self.manager) {
                    if let Some(entry) = new_state.get_entry() {
                        // TODO: optimize. This is very inefficient to retrieve and then ignore the values
                        if self.table_id == entry.table_id()
                            && bound_contains_key::<T, K>(&self.query_range, entry.key())
                        {
                            self.last = Some(new_state);
                            return self.last.as_ref().map(|s| s.get_entry().unwrap());
                        } else {
                            #[allow(clippy::collapsible_else_if)]
                            if self.reversed {
                                if let Bound::Included(start) = self.query_range.start_bound() {
                                    if entry
                                        .compare::<K>(self.table_id, start.as_bytes().as_ref())
                                        .is_lt()
                                    {
                                        self.last = None;
                                        return None;
                                    }
                                } else if let Bound::Excluded(start) =
                                    self.query_range.start_bound()
                                {
                                    if entry
                                        .compare::<K>(self.table_id, start.as_bytes().as_ref())
                                        .is_le()
                                    {
                                        self.last = None;
                                        return None;
                                    }
                                }
                            } else {
                                if let Bound::Included(end) = self.query_range.end_bound() {
                                    if entry
                                        .compare::<K>(self.table_id, end.as_bytes().as_ref())
                                        .is_gt()
                                    {
                                        self.last = None;
                                        return None;
                                    }
                                } else if let Bound::Excluded(end) = self.query_range.end_bound() {
                                    if entry
                                        .compare::<K>(self.table_id, end.as_bytes().as_ref())
                                        .is_ge()
                                    {
                                        self.last = None;
                                        return None;
                                    }
                                }
                            };

                            state = new_state;
                        }
                    } else {
                        state = new_state;
                    }
                } else {
                    self.last = None;
                    return None;
                }
            }
        }
        None
    }
}

pub trait BtreeEntry<'a: 'b, 'b> {
    fn key(&'b self) -> &'a [u8];
    fn value(&'b self) -> &'a [u8];
}

fn cmp_keys<K: RedbKey + ?Sized>(table1: u64, key1: &[u8], table2: u64, key2: &[u8]) -> Ordering {
    match table1.cmp(&table2) {
        Ordering::Less => Ordering::Less,
        Ordering::Equal => K::compare(key1, key2),
        Ordering::Greater => Ordering::Greater,
    }
}

fn bound_contains_key<'a, T: RangeBounds<&'a K>, K: RedbKey + ?Sized + 'a>(
    range: &T,
    key: &[u8],
) -> bool {
    if let Bound::Included(start) = range.start_bound() {
        if K::compare(key, start.as_bytes().as_ref()).is_lt() {
            return false;
        }
    } else if let Bound::Excluded(start) = range.start_bound() {
        if K::compare(key, start.as_bytes().as_ref()).is_le() {
            return false;
        }
    }
    if let Bound::Included(end) = range.end_bound() {
        if K::compare(key, end.as_bytes().as_ref()).is_gt() {
            return false;
        }
    } else if let Bound::Excluded(end) = range.end_bound() {
        if K::compare(key, end.as_bytes().as_ref()).is_ge() {
            return false;
        }
    }

    true
}

// Provides a simple zero-copy way to access entries
//
// Entry format is:
// * (8 bytes) key_size
// * (8 bytes) table_id, 64-bit big endian unsigned. Stored between key_size & key_data, so that
//   it can be read with key_data as a single key_size + 8 length unique key for the entire db
// * (key_size bytes) key_data
// * (8 bytes) value_size
// * (value_size bytes) value_data
pub struct EntryAccessor<'a> {
    raw: &'a [u8],
}

impl<'a> EntryAccessor<'a> {
    fn new(raw: &'a [u8]) -> Self {
        EntryAccessor { raw }
    }

    fn key_len(&self) -> usize {
        u64::from_be_bytes(self.raw[0..8].try_into().unwrap()) as usize
    }

    pub(in crate) fn table_id(&self) -> u64 {
        u64::from_be_bytes(self.raw[8..16].try_into().unwrap())
    }

    fn value_offset(&self) -> usize {
        16 + self.key_len() + 8
    }

    fn value_len(&self) -> usize {
        let key_len = self.key_len();
        u64::from_be_bytes(
            self.raw[(16 + key_len)..(16 + key_len + 8)]
                .try_into()
                .unwrap(),
        ) as usize
    }

    fn raw_len(&self) -> usize {
        16 + self.key_len() + 8 + self.value_len()
    }

    fn compare<K: RedbKey + ?Sized>(&self, table: u64, key: &[u8]) -> Ordering {
        cmp_keys::<K>(self.table_id(), self.key(), table, key)
    }
}

impl<'a: 'b, 'b> BtreeEntry<'a, 'b> for EntryAccessor<'a> {
    fn key(&'b self) -> &'a [u8] {
        &self.raw[16..(16 + self.key_len())]
    }

    fn value(&'b self) -> &'a [u8] {
        &self.raw[self.value_offset()..(self.value_offset() + self.value_len())]
    }
}

// Note the caller is responsible for ensuring that the buffer is large enough
// and rewriting all fields if any dynamically sized fields are written
struct EntryMutator<'a> {
    raw: &'a mut [u8],
}

impl<'a> EntryMutator<'a> {
    fn new(raw: &'a mut [u8]) -> Self {
        EntryMutator { raw }
    }

    fn write_table_id(&mut self, table_id: u64) {
        self.raw[8..16].copy_from_slice(&table_id.to_be_bytes());
    }

    fn write_key(&mut self, key: &[u8]) {
        self.raw[0..8].copy_from_slice(&(key.len() as u64).to_be_bytes());
        self.raw[16..(16 + key.len())].copy_from_slice(key);
    }

    fn write_value(&mut self, value: &[u8]) {
        let value_offset = EntryAccessor::new(self.raw).value_offset();
        self.raw[(value_offset - 8)..value_offset]
            .copy_from_slice(&(value.len() as u64).to_be_bytes());
        self.raw[value_offset..(value_offset + value.len())].copy_from_slice(value);
    }
}

// Provides a simple zero-copy way to access a leaf page
//
// Entry format is:
// * (1 byte) type: 1 = LEAF
// * (n bytes) lesser_entry
// * (n bytes) greater_entry: optional
struct LeafAccessor<'a: 'b, 'b, T: Page + 'a> {
    page: &'b T,
    _page_lifetime: PhantomData<&'a ()>,
}

impl<'a: 'b, 'b, T: Page + 'a> LeafAccessor<'a, 'b, T> {
    fn new(page: &'b T) -> Self {
        LeafAccessor {
            page,
            _page_lifetime: Default::default(),
        }
    }

    fn offset_of_lesser(&self) -> usize {
        1
    }

    fn offset_of_greater(&self) -> usize {
        1 + self.lesser().raw_len()
    }

    fn lesser(&self) -> EntryAccessor<'b> {
        EntryAccessor::new(&self.page.memory()[self.offset_of_lesser()..])
    }

    fn greater(&self) -> Option<EntryAccessor<'b>> {
        let entry = EntryAccessor::new(&self.page.memory()[self.offset_of_greater()..]);
        if entry.key_len() == 0 {
            None
        } else {
            Some(entry)
        }
    }
}

// Note the caller is responsible for ensuring that the buffer is large enough
// and rewriting all fields if any dynamically sized fields are written
struct LeafBuilder<'a: 'b, 'b> {
    page: &'b mut PageMut<'a>,
}

impl<'a: 'b, 'b> LeafBuilder<'a, 'b> {
    fn new(page: &'b mut PageMut<'a>) -> Self {
        page.memory_mut()[0] = LEAF;
        LeafBuilder { page }
    }

    fn write_lesser(&mut self, table_id: u64, key: &[u8], value: &[u8]) {
        let mut entry = EntryMutator::new(&mut self.page.memory_mut()[1..]);
        entry.write_table_id(table_id);
        entry.write_key(key);
        entry.write_value(value);
    }

    fn write_greater(&mut self, entry: Option<(u64, &[u8], &[u8])>) {
        let offset = 1 + EntryAccessor::new(&self.page.memory()[1..]).raw_len();
        let mut writer = EntryMutator::new(&mut self.page.memory_mut()[offset..]);
        if let Some((table_id, key, value)) = entry {
            writer.write_table_id(table_id);
            writer.write_key(key);
            writer.write_value(value);
        } else {
            writer.write_key(&[]);
        }
    }
}

// Provides a simple zero-copy way to access an index page
struct InternalAccessor<'a: 'b, 'b> {
    page: &'b PageImpl<'a>,
}

impl<'a: 'b, 'b> InternalAccessor<'a, 'b> {
    fn new(page: &'b PageImpl<'a>) -> Self {
        assert_eq!(page.memory()[0], INTERNAL);
        InternalAccessor { page }
    }

    fn key_offset(&self, n: usize) -> usize {
        let offset = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) * 2 + 8 * n;
        u64::from_be_bytes(self.page.memory()[offset..(offset + 8)].try_into().unwrap()) as usize
    }

    fn key_len(&self, n: usize) -> usize {
        let offset = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) + 8 * n;
        u64::from_be_bytes(self.page.memory()[offset..(offset + 8)].try_into().unwrap()) as usize
    }

    fn table_id(&self, n: usize) -> Option<u64> {
        assert!(n < BTREE_ORDER - 1);
        let len = self.key_len(n);
        if len == 0 {
            return None;
        }
        let offset = 1 + 8 * BTREE_ORDER + 8 * n;
        Some(u64::from_be_bytes(
            self.page.memory()[offset..(offset + 8)].try_into().unwrap(),
        ))
    }

    fn key(&self, n: usize) -> Option<&[u8]> {
        assert!(n < BTREE_ORDER - 1);
        let offset = self.key_offset(n);
        let len = self.key_len(n);
        if len == 0 {
            return None;
        }
        Some(&self.page.memory()[offset..(offset + len)])
    }

    fn child_page(&self, n: usize) -> Option<PageNumber> {
        assert!(n < BTREE_ORDER);
        if n > 0 && self.key_len(n - 1) == 0 {
            return None;
        }
        let offset = 1 + 8 * n;
        Some(PageNumber(u64::from_be_bytes(
            self.page.memory()[offset..(offset + 8)].try_into().unwrap(),
        )))
    }
}

// Note the caller is responsible for ensuring that the buffer is large enough
// and rewriting all fields if any dynamically sized fields are written
// Layout is:
// 1 byte: type
// repeating (BTREE_ORDER times):
// 8 bytes page number
// repeating (BTREE_ORDER - 1 times):
// * 8 bytes: table id
// repeating (BTREE_ORDER - 1 times):
// * 8 bytes: key len. Zero length indicates no key, or following page
// repeating (BTREE_ORDER - 1 times):
// * 8 bytes: key offset. Offset to the key data
// repeating (BTREE_ORDER - 1 times):
// * n bytes: key data
struct InternalBuilder<'a: 'b, 'b> {
    page: &'b mut PageMut<'a>,
}

impl<'a: 'b, 'b> InternalBuilder<'a, 'b> {
    fn new(page: &'b mut PageMut<'a>) -> Self {
        page.memory_mut()[0] = INTERNAL;
        //  ensure all the key lengths are zeroed, since we use those to indicate missing keys
        let start = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1);
        for i in 0..(BTREE_ORDER - 1) {
            let offset = start + 8 * i;
            page.memory_mut()[offset..(offset + 8)].copy_from_slice(&(0u64).to_be_bytes());
        }
        InternalBuilder { page }
    }

    fn write_first_page(&mut self, page_number: PageNumber) {
        let offset = 1;
        self.page.memory_mut()[offset..(offset + 8)].copy_from_slice(&page_number.to_be_bytes());
    }

    fn key_offset(&self, n: usize) -> usize {
        let offset = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) * 2 + 8 * n;
        u64::from_be_bytes(self.page.memory()[offset..(offset + 8)].try_into().unwrap()) as usize
    }

    fn key_len(&self, n: usize) -> usize {
        let offset = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) + 8 * n;
        u64::from_be_bytes(self.page.memory()[offset..(offset + 8)].try_into().unwrap()) as usize
    }

    // Write the nth key and page of values greater than this key, but less than or equal to the next
    // Caller must write keys & pages in increasing order
    fn write_nth_key(&mut self, table_id: u64, key: &[u8], page_number: PageNumber, n: usize) {
        assert!(n < BTREE_ORDER - 1);
        let offset = 1 + 8 * (n + 1);
        self.page.memory_mut()[offset..(offset + 8)].copy_from_slice(&page_number.to_be_bytes());

        let offset = 1 + 8 * BTREE_ORDER + 8 * n;
        self.page.memory_mut()[offset..(offset + 8)].copy_from_slice(&table_id.to_be_bytes());

        let offset = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) + 8 * n;
        self.page.memory_mut()[offset..(offset + 8)]
            .copy_from_slice(&(key.len() as u64).to_be_bytes());

        let offset = 1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) * 2 + 8 * n;
        let data_offset = if n > 0 {
            self.key_offset(n - 1) + self.key_len(n - 1)
        } else {
            1 + 8 * BTREE_ORDER + 8 * (BTREE_ORDER - 1) * 3
        };
        self.page.memory_mut()[offset..(offset + 8)]
            .copy_from_slice(&(data_offset as u64).to_be_bytes());

        self.page.memory_mut()[data_offset..(data_offset + key.len())].copy_from_slice(key);
    }
}

pub(in crate) fn tree_height<'a>(page: PageImpl<'a>, manager: &'a TransactionalMemory) -> usize {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => 1,
        INTERNAL => {
            let accessor = InternalAccessor::new(&page);
            let mut max_child_height = 0;
            for i in 0..BTREE_ORDER {
                if let Some(child) = accessor.child_page(i) {
                    let height = tree_height(manager.get_page(child), manager);
                    max_child_height = max(max_child_height, height);
                }
            }

            max_child_height + 1
        }
        _ => unreachable!(),
    }
}

pub(in crate) fn print_node(page: &PageImpl) {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => {
            let accessor = LeafAccessor::new(page);
            eprint!(
                "Leaf[ (page={}), lt_table={} lt_key={:?}",
                page.get_page_number().0,
                accessor.lesser().table_id(),
                accessor.lesser().key()
            );
            if let Some(greater) = accessor.greater() {
                eprint!(
                    " gt_table={} gt_key={:?}",
                    greater.table_id(),
                    greater.key()
                );
            }
            eprint!("]");
        }
        INTERNAL => {
            let accessor = InternalAccessor::new(page);
            eprint!(
                "Internal[ (page={}), child_0={}",
                page.get_page_number().0,
                accessor.child_page(0).unwrap().0
            );
            for i in 0..(BTREE_ORDER - 1) {
                if let Some(child) = accessor.child_page(i + 1) {
                    let table = accessor.table_id(i).unwrap();
                    let key = accessor.key(i).unwrap();
                    eprint!(" table_{}={}", i, table);
                    eprint!(" key_{}={:?}", i, key);
                    eprint!(" child_{}={}", i + 1, child.0);
                }
            }
            eprint!("]");
        }
        _ => unreachable!(),
    }
}

pub(in crate) fn node_children<'a>(
    page: &PageImpl<'a>,
    manager: &'a TransactionalMemory,
) -> Vec<PageImpl<'a>> {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => {
            vec![]
        }
        INTERNAL => {
            let mut children = vec![];
            let accessor = InternalAccessor::new(&page);
            for i in 0..BTREE_ORDER {
                if let Some(child) = accessor.child_page(i) {
                    children.push(manager.get_page(child));
                }
            }
            children
        }
        _ => unreachable!(),
    }
}

pub(in crate) fn print_tree<'a>(page: PageImpl<'a>, manager: &'a TransactionalMemory) {
    let mut pages = vec![page];
    while !pages.is_empty() {
        let mut next_children = vec![];
        for page in pages.drain(..) {
            next_children.extend(node_children(&page, manager));
            print_node(&page);
            eprint!("  ");
        }
        eprintln!();

        pages = next_children;
    }
}

pub(in crate) fn tree_delete<'a, K: RedbKey + ?Sized>(
    page: PageImpl<'a>,
    table: u64,
    key: &[u8],
    manager: &'a TransactionalMemory,
) -> Option<PageNumber> {
    match tree_delete_helper::<K>(page, table, key, manager) {
        DeletionResult::Subtree(page) => Some(page),
        DeletionResult::PartialLeaf(entries) => {
            assert!(entries.is_empty());
            None
        }
        DeletionResult::PartialInternal(pages) => {
            assert_eq!(pages.len(), 1);
            Some(pages[0])
        }
    }
}

enum DeletionResult {
    // A proper subtree
    Subtree(PageNumber),
    // A leaf subtree with too few entries
    PartialLeaf(Vec<(u64, Vec<u8>, Vec<u8>)>),
    // A index page subtree with too few children
    PartialInternal(Vec<PageNumber>),
}

// Must return the pages in order
fn split_leaf(
    leaf: PageNumber,
    partial: &[(u64, Vec<u8>, Vec<u8>)],
    manager: &TransactionalMemory,
) -> Option<(PageNumber, PageNumber)> {
    assert!(partial.is_empty());
    let page = manager.get_page(leaf);
    let accessor = LeafAccessor::new(&page);
    if let Some(greater) = accessor.greater() {
        let lesser = accessor.lesser();
        let page1 = make_single_leaf(lesser.table_id(), lesser.key(), lesser.value(), manager);
        let page2 = make_single_leaf(greater.table_id(), greater.key(), greater.value(), manager);
        Some((page1, page2))
    } else {
        None
    }
}

fn merge_leaf(
    leaf: PageNumber,
    partial: &[(u64, Vec<u8>, Vec<u8>)],
    manager: &TransactionalMemory,
) -> PageNumber {
    let page = manager.get_page(leaf);
    let accessor = LeafAccessor::new(&page);
    assert!(accessor.greater().is_none());
    assert!(partial.is_empty());
    leaf
}

// Must return the pages in order
fn split_index(
    index: PageNumber,
    partial: &[PageNumber],
    manager: &TransactionalMemory,
) -> Option<(PageNumber, PageNumber)> {
    assert_eq!(partial.len(), 1);
    assert_eq!(BTREE_ORDER, 3);
    let page = manager.get_page(index);
    let accessor = InternalAccessor::new(&page);

    accessor.child_page(BTREE_ORDER - 1)?;

    let mut pages = vec![];
    pages.extend_from_slice(partial);
    for i in 0..BTREE_ORDER {
        if let Some(page_number) = accessor.child_page(i) {
            pages.push(page_number);
        }
    }

    pages.sort_by_key(|p| max_table_key(manager.get_page(*p), manager));
    assert_eq!(pages.len(), 4);

    let (table, key) = max_table_key(manager.get_page(pages[0]), manager);
    let page1 = make_index(table, &key, pages[0], pages[1], manager);
    let (table, key) = max_table_key(manager.get_page(pages[2]), manager);
    let page2 = make_index(table, &key, pages[2], pages[3], manager);

    Some((page1, page2))
}

fn merge_index(
    index: PageNumber,
    partial: &[PageNumber],
    manager: &TransactionalMemory,
) -> PageNumber {
    assert_eq!(partial.len(), 1);
    assert_eq!(BTREE_ORDER, 3);
    let page = manager.get_page(index);
    let accessor = InternalAccessor::new(&page);
    assert!(accessor.child_page(BTREE_ORDER - 1).is_none());

    let mut pages = vec![];
    pages.extend_from_slice(partial);
    for i in 0..BTREE_ORDER {
        if let Some(page_number) = accessor.child_page(i) {
            pages.push(page_number);
        }
    }

    pages.sort_by_key(|p| max_table_key(manager.get_page(*p), manager));
    assert!(pages.len() <= BTREE_ORDER);

    let mut page = manager.allocate();
    let mut builder = InternalBuilder::new(&mut page);
    builder.write_first_page(pages[0]);
    for i in 0..(pages.len() - 1) {
        let (table, key) = max_table_key(manager.get_page(pages[i]), manager);
        let next_child = pages[i + 1];
        builder.write_nth_key(table, &key, next_child, i);
    }

    page.get_page_number()
}

fn repair_children(
    children: Vec<DeletionResult>,
    manager: &TransactionalMemory,
) -> Vec<PageNumber> {
    if children.iter().all(|x| matches!(x, Subtree(_))) {
        children
            .iter()
            .map(|x| match x {
                Subtree(page_number) => *page_number,
                _ => unreachable!(),
            })
            .collect()
    } else if children.iter().any(|x| matches!(x, PartialLeaf(_))) {
        let mut result = vec![];
        let mut repaired = false;
        for i in 0..children.len() {
            if let Subtree(page_number) = &children[i] {
                if repaired {
                    result.push(*page_number);
                    continue;
                }
                if i > 0 {
                    if let Some(PartialLeaf(partials)) = children.get(i - 1) {
                        if let Some((page1, page2)) = split_leaf(*page_number, partials, manager) {
                            result.push(page1);
                            result.push(page2);
                            repaired = true;
                        } else {
                            result.push(merge_leaf(*page_number, partials, manager));
                            repaired = true;
                        }
                    }
                }
                if let Some(PartialLeaf(partials)) = children.get(i + 1) {
                    if let Some((page1, page2)) = split_leaf(*page_number, partials, manager) {
                        result.push(page1);
                        result.push(page2);
                        repaired = true;
                    } else {
                        result.push(merge_leaf(*page_number, partials, manager));
                        repaired = true;
                    }
                }
            }
        }
        result
    } else if children.iter().any(|x| matches!(x, PartialInternal(_))) {
        let mut result = vec![];
        let mut repaired = false;
        for i in 0..children.len() {
            if let Subtree(page_number) = &children[i] {
                if repaired {
                    result.push(*page_number);
                    continue;
                }
                if i > 0 {
                    if let Some(PartialInternal(partials)) = children.get(i - 1) {
                        if let Some((page1, page2)) = split_index(*page_number, partials, manager) {
                            result.push(page1);
                            result.push(page2);
                            repaired = true;
                        } else {
                            result.push(merge_index(*page_number, partials, manager));
                            repaired = true;
                        }
                    }
                }
                if let Some(PartialInternal(partials)) = children.get(i + 1) {
                    if let Some((page1, page2)) = split_index(*page_number, partials, manager) {
                        result.push(page1);
                        result.push(page2);
                        repaired = true;
                    } else {
                        result.push(merge_index(*page_number, partials, manager));
                        repaired = true;
                    }
                }
            }
        }
        result
    } else {
        unreachable!()
    }
}

fn max_table_key(page: PageImpl, manager: &TransactionalMemory) -> (u64, Vec<u8>) {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => {
            let accessor = LeafAccessor::new(&page);
            if let Some(greater) = accessor.greater() {
                (greater.table_id(), greater.key().to_vec())
            } else {
                (
                    accessor.lesser().table_id(),
                    accessor.lesser().key().to_vec(),
                )
            }
        }
        INTERNAL => {
            let accessor = InternalAccessor::new(&page);
            for i in (0..BTREE_ORDER).rev() {
                if let Some(child) = accessor.child_page(i) {
                    return max_table_key(manager.get_page(child), manager);
                }
            }
            unreachable!();
        }
        _ => unreachable!(),
    }
}

// Returns the page number of the sub-tree with this key deleted, or None if the sub-tree is empty.
// If key is not found, guaranteed not to modify the tree
#[allow(clippy::needless_return)]
fn tree_delete_helper<'a, K: RedbKey + ?Sized>(
    page: PageImpl<'a>,
    table: u64,
    key: &[u8],
    manager: &'a TransactionalMemory,
) -> DeletionResult {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => {
            let accessor = LeafAccessor::new(&page);
            #[allow(clippy::collapsible_else_if)]
            if let Some(greater) = accessor.greater() {
                if accessor.lesser().compare::<K>(table, key).is_ne()
                    && greater.compare::<K>(table, key).is_ne()
                {
                    // Not found
                    return Subtree(page.get_page_number());
                }
                let new_leaf = if accessor.lesser().compare::<K>(table, key).is_eq() {
                    (greater.table_id(), greater.key(), greater.value())
                } else {
                    (
                        accessor.lesser().table_id(),
                        accessor.lesser().key(),
                        accessor.lesser().value(),
                    )
                };

                Subtree(make_single_leaf(
                    new_leaf.0, new_leaf.1, new_leaf.2, manager,
                ))
            } else {
                if accessor.lesser().compare::<K>(table, key).is_eq() {
                    // Deleted the entire left
                    PartialLeaf(vec![])
                } else {
                    // Not found
                    Subtree(page.get_page_number())
                }
            }
        }
        INTERNAL => {
            let accessor = InternalAccessor::new(&page);
            let original_page_number = page.get_page_number();
            let mut children = vec![];
            let mut found = false;
            let mut last_valid_child = BTREE_ORDER - 1;
            for i in 0..(BTREE_ORDER - 1) {
                if let Some(index_table) = accessor.table_id(i) {
                    let index_key = accessor.key(i).unwrap();
                    let child_page = accessor.child_page(i).unwrap();
                    if cmp_keys::<K>(table, key, index_table, index_key).is_le() && !found {
                        found = true;
                        let result = tree_delete_helper::<K>(
                            manager.get_page(child_page),
                            table,
                            key,
                            manager,
                        );
                        // The key must not have been found, since the subtree didn't change
                        if let Subtree(page_number) = result {
                            if page_number == child_page {
                                return Subtree(original_page_number);
                            }
                        }
                        children.push(result);
                    } else {
                        children.push(Subtree(child_page));
                    }
                } else {
                    last_valid_child = i;
                    break;
                }
            }
            let last_page = accessor.child_page(last_valid_child).unwrap();
            if found {
                // Already found the insertion place, so just copy
                children.push(Subtree(last_page));
            } else {
                let result =
                    tree_delete_helper::<K>(manager.get_page(last_page), table, key, manager);
                found = true;
                // The key must not have been found, since the subtree didn't change
                if let Subtree(page_number) = result {
                    if page_number == last_page {
                        return Subtree(original_page_number);
                    }
                }
                children.push(result);
            }
            assert!(found);
            assert!(children.len() > 1);
            let children = repair_children(children, manager);
            if children.len() == 1 {
                return PartialInternal(children);
            }

            let mut page = manager.allocate();
            let mut builder = InternalBuilder::new(&mut page);
            builder.write_first_page(children[0]);
            for i in 0..(children.len() - 1) {
                let (table, key) = max_table_key(manager.get_page(children[i]), manager);
                let next_child = children[i + 1];
                builder.write_nth_key(table, &key, next_child, i);
            }
            Subtree(page.get_page_number())
        }
        _ => unreachable!(),
    }
}

pub(in crate) fn make_mut_single_leaf<'a>(
    table: u64,
    key: &[u8],
    value: &[u8],
    manager: &'a TransactionalMemory,
) -> (PageNumber, AccessGuardMut<'a>) {
    let mut page = manager.allocate();
    let mut builder = LeafBuilder::new(&mut page);
    builder.write_lesser(table, key, value);
    builder.write_greater(None);

    let accessor = LeafAccessor::new(&page);
    let offset = accessor.offset_of_lesser() + accessor.lesser().value_offset();

    let page_num = page.get_page_number();
    let guard = AccessGuardMut::new(page, offset, value.len());

    (page_num, guard)
}

pub(in crate) fn make_mut_double_leaf_right<'a, K: RedbKey + ?Sized>(
    table1: u64,
    key1: &[u8],
    value1: &[u8],
    table2: u64,
    key2: &[u8],
    value2: &[u8],
    manager: &'a TransactionalMemory,
) -> (PageNumber, AccessGuardMut<'a>) {
    debug_assert!(cmp_keys::<K>(table1, key1, table2, key2).is_lt());
    let mut page = manager.allocate();
    let mut builder = LeafBuilder::new(&mut page);
    builder.write_lesser(table1, key1, value1);
    builder.write_greater(Some((table2, key2, value2)));

    let accessor = LeafAccessor::new(&page);
    let offset = accessor.offset_of_greater() + accessor.greater().unwrap().value_offset();

    let page_num = page.get_page_number();
    let guard = AccessGuardMut::new(page, offset, value2.len());

    (page_num, guard)
}

pub(in crate) fn make_mut_double_leaf_left<'a, K: RedbKey + ?Sized>(
    table1: u64,
    key1: &[u8],
    value1: &[u8],
    table2: u64,
    key2: &[u8],
    value2: &[u8],
    manager: &'a TransactionalMemory,
) -> (PageNumber, AccessGuardMut<'a>) {
    debug_assert!(cmp_keys::<K>(table1, key1, table2, key2).is_lt());
    let mut page = manager.allocate();
    let mut builder = LeafBuilder::new(&mut page);
    builder.write_lesser(table1, key1, value1);
    builder.write_greater(Some((table2, key2, &value2)));

    let accessor = LeafAccessor::new(&page);
    let offset = accessor.offset_of_lesser() + accessor.lesser().value_offset();

    let page_num = page.get_page_number();
    let guard = AccessGuardMut::new(page, offset, value1.len());

    (page_num, guard)
}

pub(in crate) fn make_single_leaf<'a>(
    table: u64,
    key: &[u8],
    value: &[u8],
    manager: &'a TransactionalMemory,
) -> PageNumber {
    let mut page = manager.allocate();
    let mut builder = LeafBuilder::new(&mut page);
    builder.write_lesser(table, key, value);
    builder.write_greater(None);
    page.get_page_number()
}

pub(in crate) fn make_index(
    table: u64,
    key: &[u8],
    lte_page: PageNumber,
    gt_page: PageNumber,
    manager: &TransactionalMemory,
) -> PageNumber {
    let mut page = manager.allocate();
    let mut builder = InternalBuilder::new(&mut page);
    builder.write_first_page(lte_page);
    builder.write_nth_key(table, &key, gt_page, 0);
    page.get_page_number()
}

// Returns the page number of the sub-tree into which the key was inserted,
// and the guard which can be used to access the value
pub(in crate) fn tree_insert<'a, K: RedbKey + ?Sized>(
    page: PageImpl<'a>,
    table: u64,
    key: &[u8],
    value: &[u8],
    manager: &'a TransactionalMemory,
) -> (PageNumber, AccessGuardMut<'a>) {
    let (page1, more, guard) = tree_insert_helper::<K>(page, table, key, value, manager);

    if let Some((table, key, page2)) = more {
        let index_page = make_index(table, &key, page1, page2, manager);
        (index_page, guard)
    } else {
        (page1, guard)
    }
}

#[allow(clippy::type_complexity)]
fn tree_insert_helper<'a, K: RedbKey + ?Sized>(
    page: PageImpl<'a>,
    table: u64,
    key: &[u8],
    value: &[u8],
    manager: &'a TransactionalMemory,
) -> (
    PageNumber,
    Option<(u64, Vec<u8>, PageNumber)>,
    AccessGuardMut<'a>,
) {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => {
            let accessor = LeafAccessor::new(&page);
            if let Some(entry) = accessor.greater() {
                match entry.compare::<K>(table, key) {
                    Ordering::Less => {
                        // New entry goes in a new page to the right, so leave this page untouched
                        let left_page = page.get_page_number();

                        let (right_page, guard) = make_mut_single_leaf(table, key, value, manager);

                        (
                            left_page,
                            Some((entry.table_id(), entry.key().to_vec(), right_page)),
                            guard,
                        )
                    }
                    Ordering::Equal => {
                        let (new_page, guard) = make_mut_double_leaf_right::<K>(
                            accessor.lesser().table_id(),
                            accessor.lesser().key(),
                            accessor.lesser().value(),
                            table,
                            key,
                            value,
                            manager,
                        );

                        (new_page, None, guard)
                    }
                    Ordering::Greater => {
                        let right_table = entry.table_id();
                        let right_key = entry.key();
                        let right_value = entry.value();

                        let left_table = accessor.lesser().table_id();
                        let left_key = accessor.lesser().key();
                        let left_value = accessor.lesser().value();

                        match accessor.lesser().compare::<K>(table, key) {
                            Ordering::Less => {
                                let (left, guard) = make_mut_double_leaf_right::<K>(
                                    left_table, left_key, left_value, table, key, value, manager,
                                );
                                let right =
                                    make_single_leaf(right_table, right_key, right_value, manager);

                                (left, Some((table, key.to_vec(), right)), guard)
                            }
                            Ordering::Equal => {
                                let (new_page, guard) = make_mut_double_leaf_left::<K>(
                                    table,
                                    key,
                                    value,
                                    right_table,
                                    right_key,
                                    right_value,
                                    manager,
                                );

                                (new_page, None, guard)
                            }
                            Ordering::Greater => {
                                let (left, guard) = make_mut_double_leaf_left::<K>(
                                    table, key, value, left_table, left_key, left_value, manager,
                                );
                                let right =
                                    make_single_leaf(right_table, right_key, right_value, manager);

                                (left, Some((left_table, left_key.to_vec(), right)), guard)
                            }
                        }
                    }
                }
            } else {
                let (new_page, guard) = match cmp_keys::<K>(
                    accessor.lesser().table_id(),
                    accessor.lesser().key(),
                    table,
                    key,
                ) {
                    Ordering::Less => make_mut_double_leaf_right::<K>(
                        accessor.lesser().table_id(),
                        accessor.lesser().key(),
                        accessor.lesser().value(),
                        table,
                        key,
                        value,
                        manager,
                    ),
                    Ordering::Equal => make_mut_single_leaf(table, key, value, manager),
                    Ordering::Greater => make_mut_double_leaf_left::<K>(
                        table,
                        key,
                        value,
                        accessor.lesser().table_id(),
                        accessor.lesser().key(),
                        accessor.lesser().value(),
                        manager,
                    ),
                };

                (new_page, None, guard)
            }
        }
        INTERNAL => {
            let accessor = InternalAccessor::new(&page);
            let mut children = vec![];
            let mut index_table_keys = vec![];
            let mut guard = None;
            let mut last_valid_child = BTREE_ORDER - 1;
            for i in 0..(BTREE_ORDER - 1) {
                if let Some(index_table) = accessor.table_id(i) {
                    let index_key = accessor.key(i).unwrap();
                    if cmp_keys::<K>(table, key, index_table, index_key).is_le() && guard.is_none()
                    {
                        let lte_page = accessor.child_page(i).unwrap();
                        let (page1, more, guard2) = tree_insert_helper::<K>(
                            manager.get_page(lte_page),
                            table,
                            key,
                            value,
                            manager,
                        );
                        children.push(page1);
                        if let Some((index_table, index_key, page2)) = more {
                            index_table_keys.push((index_table, index_key));
                            children.push(page2);
                        }
                        index_table_keys.push((index_table, index_key.to_vec()));
                        assert!(guard.is_none());
                        guard = Some(guard2);
                    } else {
                        children.push(accessor.child_page(i).unwrap());
                        index_table_keys.push((index_table, index_key.to_vec()));
                    }
                } else {
                    last_valid_child = i;
                    break;
                }
            }
            let last_page = accessor.child_page(last_valid_child).unwrap();
            if guard.is_some() {
                // Already found the insertion place, so just copy
                children.push(last_page);
            } else {
                let (page1, more, guard2) = tree_insert_helper::<K>(
                    manager.get_page(last_page),
                    table,
                    key,
                    value,
                    manager,
                );
                children.push(page1);
                if let Some((index_table, index_key, page2)) = more {
                    index_table_keys.push((index_table, index_key));
                    children.push(page2);
                }
                assert!(guard.is_none());
                guard = Some(guard2);
            }
            let guard = guard.unwrap();
            assert_eq!(children.len() - 1, index_table_keys.len());

            let mut page = manager.allocate();
            let mut builder = InternalBuilder::new(&mut page);
            if children.len() <= BTREE_ORDER {
                builder.write_first_page(children[0]);
                for (i, ((table, key), page_number)) in index_table_keys
                    .iter()
                    .zip(children.iter().skip(1))
                    .enumerate()
                {
                    builder.write_nth_key(*table, key, *page_number, i);
                }
                (page.get_page_number(), None, guard)
            } else {
                assert_eq!(BTREE_ORDER, 3);
                builder.write_first_page(children[0]);
                let (table, key) = &index_table_keys[0];
                builder.write_nth_key(*table, &key, children[1], 0);

                let (index_table, index_key) = &index_table_keys[1];

                let mut page2 = manager.allocate();
                let mut builder2 = InternalBuilder::new(&mut page2);
                builder2.write_first_page(children[2]);
                let (table, key) = &index_table_keys[2];
                builder2.write_nth_key(*table, &key, children[3], 0);

                (
                    page.get_page_number(),
                    Some((*index_table, index_key.to_vec(), page2.get_page_number())),
                    guard,
                )
            }
        }
        _ => unreachable!(),
    }
}

// Returns the (offset, len) of the value for the queried key, if present
pub(in crate) fn lookup_in_raw<'a, K: RedbKey + ?Sized>(
    page: PageImpl<'a>,
    table: u64,
    query: &[u8],
    manager: &'a TransactionalMemory,
) -> Option<(PageImpl<'a>, usize, usize)> {
    let node_mem = page.memory();
    match node_mem[0] {
        LEAF => {
            let accessor = LeafAccessor::new(&page);
            match cmp_keys::<K>(
                table,
                query,
                accessor.lesser().table_id(),
                accessor.lesser().key(),
            ) {
                Ordering::Less => None,
                Ordering::Equal => {
                    let offset = accessor.offset_of_lesser() + accessor.lesser().value_offset();
                    let value_len = accessor.lesser().value().len();
                    Some((page, offset, value_len))
                }
                Ordering::Greater => {
                    if let Some(entry) = accessor.greater() {
                        if entry.compare::<K>(table, query).is_eq() {
                            let offset = accessor.offset_of_greater() + entry.value_offset();
                            let value_len = entry.value().len();
                            Some((page, offset, value_len))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            }
        }
        INTERNAL => {
            let accessor = InternalAccessor::new(&page);
            for i in 0..(BTREE_ORDER - 1) {
                if let Some(table_id) = accessor.table_id(i) {
                    let key = accessor.key(i).unwrap();
                    if cmp_keys::<K>(table, query, table_id, key).is_le() {
                        let lte_page = accessor.child_page(i).unwrap();
                        return lookup_in_raw::<K>(
                            manager.get_page(lte_page),
                            table,
                            query,
                            manager,
                        );
                    }
                } else {
                    let gt_page = accessor.child_page(i).unwrap();
                    return lookup_in_raw::<K>(manager.get_page(gt_page), table, query, manager);
                }
            }
            let last_page = accessor.child_page(BTREE_ORDER - 1).unwrap();
            return lookup_in_raw::<K>(manager.get_page(last_page), table, query, manager);
        }
        _ => unreachable!(),
    }
}
