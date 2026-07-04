use std::sync::Arc;
use subvol_core::btree::{
    Btree, BtreeRoot, NodeCache,
    key::{BtreeKey, BchVal, KeyType},
    node::{BtreeNode, entry_size, BsetTree},
    iter::{BtreeIter, IterFlags},
};

fn main() {
    let cache = Arc::new(NodeCache::new());
    
    let mut left = BtreeNode::new_leaf();
    left.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
    left.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
    left.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
    println!("Left leaf key_count after inserts: {}", left.packed_keys + left.unpacked_keys);
    let left = Arc::new(left);
    
    let mut right = BtreeNode::new_leaf();
    right.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
    right.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
    println!("Right leaf key_count after inserts: {}", right.packed_keys + right.unpacked_keys);
    let right = Arc::new(right);
    
    cache.insert(1, left);
    cache.insert(2, right);
    
    // Verify cache
    println!("Cache size: {}", cache.len());
    let l = cache.get(1).unwrap();
    println!("Left leaf from cache: key_count={}", l.packed_keys + l.unpacked_keys);
    
    let es = entry_size();
    let mut internal = BtreeNode::new_internal();
    let left_min = BtreeKey::MIN_KEY;
    internal.write_entry(0, &left_min, &BchVal::new(1, 0));
    internal.write_entry(es, &BtreeKey::new(40, 1, KeyType::Normal), &BchVal::new(2, 0));
    internal.sets[0] = BsetTree {
        data_offset: 0, end_offset: es * 2, aux_offset: 0, size: 2, extra: 0,
    };
    internal.packed_keys = 2;
    internal.unpacked_keys = 0;
    
    let root = BtreeRoot { node: Arc::new(internal), depth: 1 };
    println!("Root depth: {}, key_count: {}", root.depth, root.node.packed_keys + root.node.unpacked_keys);
    
    // Test: lookup with init
    let iter = BtreeIter::init(&root, &BtreeKey::new(15, 1, KeyType::Normal), IterFlags::default(), &cache);
    let result = iter.peek();
    println!("Lookup key=15: {:?}", result.map(|(k, v)| (k.get_vaddr(), v.paddr.get())));
    
    // Test: find_child_node
    let (addr, idx) = BtreeIter::find_child_node(&root.node, &BtreeKey::new(15, 1, KeyType::Normal));
    println!("find_child_node(15) = (addr={}, idx={})", addr, idx);
    
    // Now test with Btree
    let mut b = Btree::from_root(root, cache.clone(), crate::btree::BtreeId::Extents);
    println!("Btree key_count: {}", b.root().node.packed_keys + b.root().node.unpacked_keys);
    println!("Btree depth: {}", b.depth());
    
    // Insert
    let inserted = b.insert(BtreeKey::new(15, 1, KeyType::Normal), BchVal::new(150, 0));
    println!("Insert key=15: {}", inserted);
    println!("After insert, key_count: {}", b.root().node.packed_keys + b.root().node.unpacked_keys);
    
    // Try get
    let found = b.get(&BtreeKey::new(15, 1, KeyType::Normal));
    println!("get(15): {:?}", found.map(|(k, v)| (k.get_vaddr(), v.paddr.get())));
    println!("Found is Some: {}", found.is_some());
    
    // Manual iter
    let iter2 = BtreeIter::init(b.root(), &BtreeKey::new(15, 1, KeyType::Normal), IterFlags::default(), b.cache());
    let result2 = iter2.peek();
    println!("Manual iter peek after insert: {:?}", result2.map(|(k, v)| (k.get_vaddr(), v.paddr.get())));
    
    // Check cache content
    println!("Cache size after insert: {}", b.cache().len());
    if let Some(ln) = b.cache().get(1) {
        println!("Left leaf now: key_count={}", ln.packed_keys + ln.unpacked_keys);
    }
}
