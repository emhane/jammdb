use std::collections::HashMap;
use std::fs::File;
use std::pin::Pin;

use crate::cursor::{Cursor, PageNode, PageNodeID};
use crate::data::{BucketData, Data, KVPair};
use crate::errors::{Error, Result};
use crate::node::{Branch, Node, NodeData, NodeID};
use crate::page::{Page, PageID};
use crate::ptr::Ptr;
use crate::transaction::TransactionInner;

/// A collection of data
///
/// Buckets contain a collection of data, sorted by key.
/// The data can either be key / value pairs, or nested buckets.
/// You can use buckets to [`get`](#method.get) and [`put`](#method.put) data,
/// as well as [`get`](#method.get_bucket) and [`create`](#method.create_bucket)
/// nested buckets.
///
/// You can use a [`Cursor`] to iterate over
/// all the data in a bucket.
///
/// Buckets have an inner auto-incremented counter that keeps track
/// of how many unique keys have been inserted into the bucket.
/// You can access that using the [`next_int()`](#method.next_int) function.
///
/// # Examples
///
/// ```no_run
/// use jammdb::{DB, Data};
/// # use jammdb::Error;
///
/// # fn main() -> Result<(), Error> {
/// let mut db = DB::open("my.db")?;
/// let mut tx = db.tx(true)?;
///
/// // create a root-level bucket
/// let bucket = tx.create_bucket("my-bucket")?;
///
/// // create nested bucket
/// bucket.create_bucket("nested-bucket")?;
///
/// // insert a key / value pair (using &str)
/// bucket.put("key", "value");
///
/// // insert a key / value pair (using [u8])
/// bucket.put([1,2,3], [4,5,6]);
///
/// for data in bucket.cursor() {
///     match data {
///         Data::Bucket(b) => println!("found a bucket with the name {:?}", b.name()),
///         Data::KeyValue(kv) => println!("found a kv pair {:?} {:?}", kv.key(), kv.value()),
///     }
/// }
///
/// println!("Bucket next_int {:?}", bucket.next_int());
/// # Ok(())
/// # }
/// ```
pub struct Bucket {
	pub(crate) tx: Ptr<TransactionInner>,
	pub(crate) meta: BucketMeta,
	pub(crate) root: PageNodeID,
	dirty: bool,
	buckets: HashMap<Vec<u8>, Pin<Box<Bucket>>>,
	nodes: Vec<Pin<Box<Node>>>,
	page_node_ids: HashMap<PageID, NodeID>,
	page_parents: HashMap<PageID, PageID>,
}

impl Bucket {
	pub(crate) fn root(tx: Ptr<TransactionInner>) -> Bucket {
		let meta = tx.meta.root;
		Bucket {
			tx,
			meta,
			root: PageNodeID::Page(meta.root_page),
			dirty: false,
			buckets: HashMap::new(),
			nodes: Vec::new(),
			page_node_ids: HashMap::new(),
			page_parents: HashMap::new(),
		}
	}

	fn new_child(&mut self, name: &[u8]) {
		let b = Bucket {
			tx: Ptr::new(&self.tx),
			meta: BucketMeta::default(),
			root: PageNodeID::Node(0),
			dirty: true,
			buckets: HashMap::new(),
			nodes: Vec::new(),
			page_node_ids: HashMap::new(),
			page_parents: HashMap::new(),
		};
		self.buckets.insert(Vec::from(name), Pin::new(Box::new(b)));
		let b = self.buckets.get_mut(name).unwrap();
		let n = Node::new(0, Page::TYPE_LEAF, Ptr::new(b));

		b.nodes.push(Pin::new(Box::new(n)));
		b.page_node_ids.insert(0, 0);
	}

	pub(crate) fn new_node(&mut self, data: NodeData) -> &mut Node {
		let node_id = self.nodes.len();
		let n = Node::with_data(node_id, data, Ptr::new(self));
		self.nodes.push(Pin::new(Box::new(n)));
		self.nodes.get_mut(node_id).unwrap()
	}

	fn from_meta(&self, meta: BucketMeta) -> Bucket {
		Bucket {
			tx: Ptr::new(&self.tx),
			meta,
			root: PageNodeID::Page(meta.root_page),
			dirty: false,
			buckets: HashMap::new(),
			nodes: Vec::new(),
			page_node_ids: HashMap::new(),
			page_parents: HashMap::new(),
		}
	}

	/// Gets an already created bucket.
	///
	/// Returns an error if
	/// 1. the given key does not exist, or
	/// 2. the key is for key / value data, not a bucket.
	///
	/// # Examples
	///
	/// ```no_run
	/// use jammdb::{DB};
	/// # use jammdb::Error;
	///
	/// # fn main() -> Result<(), Error> {
	/// let mut db = DB::open("my.db")?;
	/// let mut tx = db.tx(false)?;
	///
	/// // get a root-level bucket
	/// let bucket = tx.get_bucket("my-bucket")?;
	///
	/// // get nested bucket
	/// let sub_bucket = bucket.get_bucket("nested-bucket")?;
	///
	/// // get nested bucket
	/// let sub_sub_bucket = sub_bucket.get_bucket("double-nested-bucket")?;
	///
	/// # Ok(())
	/// # }
	/// ```
	pub fn get_bucket<T: AsRef<[u8]>>(&mut self, name: T) -> Result<&mut Bucket> {
		let name = name.as_ref();
		let key = Vec::from(name);
		if !self.buckets.contains_key(&key) {
			let mut c = self.cursor();
			let exists = c.seek(name);
			if !exists {
				return Err(Error::BucketMissing);
			}
			match c.current() {
				Some(data) => match data {
					Data::Bucket(data) => {
						let mut b = self.from_meta(data.meta());
						b.meta = data.meta();
						b.dirty = false;
						self.buckets.insert(key.clone(), Pin::new(Box::new(b)));
					}
					_ => return Err(Error::IncompatibleValue),
				},
				None => return Err(Error::BucketMissing),
			}
		}
		Ok(self.buckets.get_mut(&key).unwrap())
	}

	/// Creates a new bucket.
	///
	/// Returns an error if the given key already exists.
	///
	/// # Examples
	///
	/// ```no_run
	/// use jammdb::{DB};
	/// # use jammdb::Error;
	///
	/// # fn main() -> Result<(), Error> {
	/// let mut db = DB::open("my.db")?;
	/// let mut tx = db.tx(true)?;
	///
	/// // create a root-level bucket
	/// let bucket = tx.create_bucket("my-bucket")?;
	///
	/// // create nested bucket
	/// let sub_bucket = bucket.create_bucket("nested-bucket")?;
	///
	/// // create nested bucket
	/// let sub_sub_bucket = sub_bucket.create_bucket("double-nested-bucket")?;
	///
	/// # Ok(())
	/// # }
	/// ```
	pub fn create_bucket<T: AsRef<[u8]>>(&mut self, name: T) -> Result<&mut Bucket> {
		if !self.tx.writable {
			return Err(Error::ReadOnlyTx);
		}
		self.dirty = true;
		let mut c = self.cursor();
		let name = name.as_ref();
		let exists = c.seek(name);
		if exists {
			if c.current().unwrap().is_kv() {
				return Err(Error::IncompatibleValue);
			}
			return Err(Error::BucketExists);
		}
		self.meta.next_int += 1;
		let key = Vec::from(name);
		self.new_child(&key);

		let data;
		{
			let b = self.buckets.get(&key).unwrap();
			let key = self.tx.copy_data(name);
			data = Data::Bucket(BucketData::from_meta(key, &b.meta));
		}

		let node = self.node(c.current_id());
		node.insert_data(data);
		let b = self.buckets.get_mut(&key).unwrap();
		Ok(b)
	}

	/// Returns the next integer for the bucket.
	/// The integer is automatically incremented each time a new key is added to the bucket.
	/// You can it as a unique key for the bucket, since it will increment each time you add something new.
	/// It will not increment if you [`put`](#method.put) a key that already exists
	///
	/// # Examples
	///
	/// ```no_run
	/// use jammdb::{DB};
	/// # use jammdb::Error;
	///
	/// # fn main() -> Result<(), Error> {
	/// let mut db = DB::open("my.db")?;
	/// let mut tx = db.tx(true)?;
	///
	/// // create a root-level bucket
	/// let bucket = tx.create_bucket("my-bucket")?;
	/// // starts at 0
	/// assert_eq!(bucket.next_int(), 0);
	///
	/// bucket.put(bucket.next_int().to_be_bytes(), [0]);
	/// // auto-incremented after inserting a key / value pair
	/// assert_eq!(bucket.next_int(), 1);
	///
	/// bucket.put(0_u64.to_be_bytes(), [0, 0]);
	/// // not incremented after updating a key / value pair
	/// assert_eq!(bucket.next_int(), 1);
	///
	/// bucket.create_bucket("nested-bucket")?;
	/// // auto-incremented after creating a nested bucket
	/// assert_eq!(bucket.next_int(), 2);
	///
	/// # Ok(())
	/// # }
	/// ```
	pub fn next_int(&self) -> u64 {
		self.meta.next_int
	}

	/// Gets data from a bucket.
	///
	/// # Examples
	///
	/// ```no_run
	/// use jammdb::{DB, Data};
	/// # use jammdb::Error;
	///
	/// # fn main() -> Result<(), Error> {
	/// let mut db = DB::open("my.db")?;
	/// let mut tx = db.tx(false)?;
	///
	/// let bucket = tx.get_bucket("my-bucket")?;
	///
	/// match bucket.get("some-key") {
	///     Some(data) => {
	///         match data {
	///             Data::Bucket(b) => println!("found a bucket with the name {:?}", b.name()),
	///             Data::KeyValue(kv) => println!("found a kv pair {:?} {:?}", kv.key(), kv.value()),
	///         }
	///     },
	///     None => println!("Key does not exist"),
	/// }
	///
	/// # Ok(())
	/// # }
	/// ```
	pub fn get<T: AsRef<[u8]>>(&self, key: T) -> Option<Data> {
		let mut c = self.cursor();
		let exists = c.seek(key);
		if exists {
			c.current()
		} else {
			None
		}
	}

	/// Adds to or replaces key / value data in the bucket.
	/// Returns an error if the key currently exists but is a bucket instead of a key / value pair.
	///
	/// # Examples
	///
	/// ```no_run
	/// use jammdb::{DB};
	/// # use jammdb::Error;
	///
	/// # fn main() -> Result<(), Error> {
	/// let mut db = DB::open("my.db")?;
	/// let mut tx = db.tx(true)?;
	///
	/// // create a root-level bucket
	/// let bucket = tx.create_bucket("my-bucket")?;
	///
	/// // insert data
	/// bucket.put("123", "456")?;
	///
	/// // update data
	/// bucket.put("123", "789")?;
	///
	/// bucket.create_bucket("nested-bucket")?;
	///
	/// assert!(bucket.put("nested-bucket", "data").is_err());
	///
	/// # Ok(())
	/// # }
	/// ```
	pub fn put<T: AsRef<[u8]>, S: AsRef<[u8]>>(&mut self, key: T, value: S) -> Result<()> {
		if !self.tx.writable {
			return Err(Error::ReadOnlyTx);
		}
		let k = self.tx.copy_data(key.as_ref());
		let v = self.tx.copy_data(value.as_ref());
		self.put_data(Data::KeyValue(KVPair::from_slice_parts(k, v)))?;
		Ok(())
	}

	/// Deletes a key-value pair from the bucket
	pub fn delete<T: AsRef<[u8]>>(&mut self, key: T) -> Result<Data> {
		let mut c = self.cursor();
		let exists = c.seek(key);
		if exists {
			let data = c.current().unwrap();
			if data.is_kv() {
				self.dirty = true;
				let node = self.node(c.current_id());
				Ok(node.delete(c.current_index()))
			} else {
				Err(Error::IncompatibleValue)
			}
		} else {
			Err(Error::KeyValueMissing)
		}
	}

	fn put_data(&mut self, data: Data) -> Result<()> {
		let mut c = self.cursor();
		let exists = c.seek(data.key());
		if exists {
			let current = c.current().unwrap();
			if current.is_kv() != data.is_kv() {
				return Err(Error::IncompatibleValue);
			}
		} else {
			self.meta.next_int += 1;
		}
		let node = self.node(c.current_id());
		node.insert_data(data);
		self.dirty = true;
		Ok(())
	}

	/// Get a cursor to iterate over the bucket.
	///
	///
	/// # Examples
	///
	/// ```no_run
	/// use jammdb::{DB, Data};
	/// # use jammdb::Error;
	///
	/// # fn main() -> Result<(), Error> {
	/// let mut db = DB::open("my.db")?;
	/// let mut tx = db.tx(false)?;
	///
	/// let bucket = tx.get_bucket("my-bucket")?;
	///
	/// for data in bucket.cursor() {
	///     match data {
	///         Data::Bucket(b) => println!("found a bucket with the name {:?}", b.name()),
	///         Data::KeyValue(kv) => println!("found a kv pair {:?} {:?}", kv.key(), kv.value()),
	///     }
	/// }
	///
	/// # Ok(())
	/// # }
	/// ```
	pub fn cursor(&self) -> Cursor {
		Cursor::new(Ptr::new(self))
	}

	pub(crate) fn page_node(&self, page: PageID) -> PageNode {
		if let Some(node_id) = self.page_node_ids.get(&page) {
			PageNode::Node(Ptr::new(self.nodes.get(*node_id).unwrap()))
		} else {
			PageNode::Page(Ptr::new(self.tx.page(page)))
		}
	}

	pub(crate) fn add_page_parent(&mut self, page: PageID, parent: PageID) {
		debug_assert!(self.meta.root_page == parent || self.page_parents.contains_key(&parent));
		self.page_parents.insert(page, parent);
	}

	pub(crate) fn node(&mut self, id: PageNodeID) -> &mut Node {
		let id: NodeID = match id {
			PageNodeID::Page(page_id) => {
				if let Some(node_id) = self.page_node_ids.get(&page_id) {
					return &mut self.nodes[*node_id as usize];
				}
				debug_assert!(
					self.meta.root_page == page_id || self.page_parents.contains_key(&page_id)
				);
				let node_id = self.nodes.len();
				self.page_node_ids.insert(page_id, node_id);
				let n: Node = Node::from_page(node_id, Ptr::new(self), self.tx.page(page_id));
				self.nodes.push(Pin::new(Box::new(n)));
				if self.meta.root_page != page_id {
					let node_key = self.nodes[node_id].data.key_parts();
					let parent = self.node(PageNodeID::Page(self.page_parents[&page_id]));
					parent.insert_child(node_id, node_key);
				}
				node_id
			}
			PageNodeID::Node(id) => id,
		};
		self.nodes.get_mut(id).unwrap()
	}

	pub(crate) fn rebalance(&mut self) -> Result<BucketMeta> {
		let mut bucket_metas = HashMap::new();
		for (key, b) in self.buckets.iter_mut() {
			if b.dirty {
				self.dirty = true;
				let bucket_meta = b.rebalance()?;
				bucket_metas.insert(key.clone(), bucket_meta);
			}
		}
		for (k, b) in bucket_metas {
			let name = self.tx.copy_data(&k[..]);
			let meta = self.tx.copy_data(b.as_ref());
			self.put_data(Data::Bucket(BucketData::from_slice_parts(name, meta)))?;
		}
		if self.dirty {
			// merge emptyish nodes first
			{
				let mut root_node = self.node(self.root);
				let should_merge_root = root_node.merge();
				// check if the root is a bucket and only has one node
				if should_merge_root && !root_node.leaf() && root_node.data.len() == 1 {
					// remove the branch and make the leaf node the root
					root_node.free_page();
					root_node.deleted = true;
					let page_id = match &root_node.data {
						NodeData::Branches(branches) => branches[0].page,
						_ => panic!("uh wat"),
					};
					self.meta.root_page = page_id;
					self.root = PageNodeID::Page(page_id);
					// if the new root hasn't been modified, no need to split it
					if !self.page_node_ids.contains_key(&page_id) {
						self.dirty = false;
						return Ok(self.meta);
					}
					// otherwise we'll continue to possibly split the new root
					// this could result in re-adding a branch root node,
					// but it's pretty unlikely!
				}
			}
			// split overflowing nodes
			{
				let mut root_node = self.node(self.root);
				while let Some(mut branches) = root_node.split() {
					branches.insert(0, Branch::from_node(root_node));
					root_node = self.new_node(NodeData::Branches(branches));
				}
				let page_id = root_node.page_id;
				self.root = PageNodeID::Node(root_node.page_id);
				self.meta.root_page = page_id;
			}
		}
		Ok(self.meta)
	}

	pub(crate) fn write(&mut self, file: &mut File) -> Result<()> {
		for (_, b) in self.buckets.iter_mut() {
			b.write(file)?;
		}
		if self.dirty {
			for node in self.nodes.iter_mut() {
				if !node.deleted {
					node.write(file)?;
				}
			}
		}
		Ok(())
	}

	#[doc(hidden)]
	#[cfg_attr(tarpaulin, skip)]
	pub fn print(&self) {
		let page = self.tx.page(self.meta.root_page);
		page.print(&self.tx);
	}
}

const META_SIZE: usize = std::mem::size_of::<BucketMeta>();

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BucketMeta {
	pub(crate) root_page: PageID,
	pub(crate) next_int: u64,
}

impl AsRef<[u8]> for BucketMeta {
	#[inline]
	fn as_ref(&self) -> &[u8] {
		let ptr = self as *const BucketMeta as *const u8;
		unsafe { std::slice::from_raw_parts(ptr, META_SIZE) }
	}
}
