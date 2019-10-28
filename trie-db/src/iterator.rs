// Copyright 2017, 2019 Parity Technologies
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::{CError, DBValue, Result, TrieError, TrieHash, TrieIterator, TrieLayout};
use hash_db::Hasher;
use triedb::TrieDB;
use node::{Node, OwnedNode};
use node_codec::NodeCodec;
use nibble::{NibbleSlice, NibbleVec, nibble_ops};

#[cfg(feature = "std")]
use ::std::borrow::Cow;
#[cfg(not(feature = "std"))]
use ::alloc::borrow::Cow;
#[cfg(feature = "std")]
use ::std::rc::Rc;
#[cfg(not(feature = "std"))]
use ::alloc::rc::Rc;
#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

#[cfg_attr(feature = "std", derive(Debug))]
#[derive(Clone, Eq, PartialEq)]
enum Status {
    Entering,
    At,
    AtChild(usize),
    Exiting,
}

#[cfg_attr(feature = "std", derive(Debug))]
#[derive(Eq, PartialEq)]
struct Crumb {
    node: Rc<OwnedNode>,
    status: Status,
}

impl Crumb {
    /// Move on to next status in the node's sequence.
    fn increment(&mut self) {
        self.status = match (&self.status, self.node.as_ref()) {
            (&Status::Entering, &OwnedNode::Extension(..)) => Status::At,
            (&Status::Entering, &OwnedNode::Branch(..))
            | (&Status::Entering, &OwnedNode::NibbledBranch(..)) => Status::At,
            (&Status::At, &OwnedNode::Branch(..))
            | (&Status::At, &OwnedNode::NibbledBranch(..)) => Status::AtChild(0),
            (&Status::AtChild(x), &OwnedNode::Branch(..))
            | (&Status::AtChild(x), &OwnedNode::NibbledBranch(..))
            if x < (nibble_ops::NIBBLE_LENGTH - 1) => Status::AtChild(x + 1),
            _ => Status::Exiting,
        }
    }
}

/// Iterator for going through all nodes in the trie in pre-order traversal order.
pub struct TrieDBNodeIterator<'a, L: TrieLayout> {
    db: &'a TrieDB<'a, L>,
    trail: Vec<Crumb>,
    key_nibbles: NibbleVec,
}

impl<'a, L: TrieLayout> TrieDBNodeIterator<'a, L> {
    /// Create a new iterator.
    pub fn new(db: &'a TrieDB<L>) -> Result<TrieDBNodeIterator<'a, L>, TrieHash<L>, CError<L>> {
        let mut r = TrieDBNodeIterator {
            db,
            trail: Vec::with_capacity(8),
            key_nibbles: NibbleVec::new(),
        };
        db.root_data().and_then(|root_data| r.descend(&root_data))?;
        Ok(r)
    }

    fn seek<'key>(
        &mut self,
        node_data: &DBValue,
        key: NibbleSlice<'key>,
    ) -> Result<(), TrieHash<L>, CError<L>> {
        let mut node_data = Cow::Borrowed(node_data);
        let mut partial = key;
        let mut full_key_nibbles = 0;
        loop {
            let data = {
                let node = L::Codec::decode(node_data.as_ref())
                    .map_err(|e| {
                        let node_hash = L::Hash::hash(node_data.as_ref());
                        Box::new(TrieError::DecoderError(node_hash, e))
                    })?;
                self.descend_into_node(node.clone().into());
                let crumb = self.trail.last_mut()
                    .expect(
                        "descend_into_node pushes a crumb onto the trial; \
                        thus the trail is non-empty; qed"
                    );

                match node {
                    Node::Leaf(slice, _) => {
                        if slice < partial {
                            crumb.status = Status::Exiting;
                        }
                        return Ok(())
                    },
                    Node::Extension(slice, item) => {
                        if !partial.starts_with(&slice) {
                            if slice < partial {
                                crumb.status = Status::Exiting;
                                self.key_nibbles.append_partial(slice.right());
                            }
                            return Ok(());
                        }

                        full_key_nibbles += slice.len();
                        partial = partial.mid(slice.len());
                        crumb.status = Status::At;
                        self.key_nibbles.append_partial(slice.right());

                        let prefix = key.back(full_key_nibbles);
                        self.db.get_raw_or_lookup(item, prefix.left())?
                    },
                    Node::Branch(nodes, _) => {
                        if partial.is_empty() {
                            return Ok(())
                        }

                        let i = partial.at(0);
                        crumb.status = Status::AtChild(i as usize);
                        self.key_nibbles.push(i);

                        if let Some(child) = nodes[i as usize] {
                            full_key_nibbles += 1;
                            partial = partial.mid(1);

                            let prefix = key.back(full_key_nibbles);
                            self.db.get_raw_or_lookup(child, prefix.left())?
                        } else {
                            return Ok(())
                        }
                    },
                    Node::NibbledBranch(slice, nodes, _) => {
                        if !partial.starts_with(&slice) {
                            if slice < partial {
                                crumb.status = Status::Exiting;
                                self.key_nibbles.append_partial(slice.right());
                                self.key_nibbles.push((nibble_ops::NIBBLE_LENGTH - 1) as u8);
                            }
                            return Ok(());
                        }

                        full_key_nibbles += slice.len();
                        partial = partial.mid(slice.len());

                        if partial.is_empty() {
                            return Ok(())
                        }

                        let i = partial.at(0);
                        crumb.status = Status::AtChild(i as usize);
                        self.key_nibbles.append_partial(slice.right());
                        self.key_nibbles.push(i);

                        if let Some(child) = nodes[i as usize] {
                            full_key_nibbles += 1;
                            partial = partial.mid(1);

                            let prefix = key.back(full_key_nibbles);
                            self.db.get_raw_or_lookup(child, prefix.left())?
                        } else {
                            return Ok(())
                        }
                    },
                    Node::Empty => {
                        if !partial.is_empty() {
                            crumb.status = Status::Exiting;
                        }
                        return Ok(())
                    },
                }
            };

            node_data = data;
        }
    }

    /// Descend into a payload.
    fn descend(&mut self, d: &[u8]) -> Result<(), TrieHash<L>, CError<L>> {
        let node_data = &self.db.get_raw_or_lookup(d, self.key_nibbles.as_prefix())?;
        let node = L::Codec::decode(&node_data)
            .map_err(|e| Box::new(TrieError::DecoderError(<TrieHash<L>>::default(), e)))?;
        Ok(self.descend_into_node(node.into()))
    }

    /// Descend into a payload.
    fn descend_into_node(&mut self, node: OwnedNode) {
        self.trail.push(Crumb {
            status: Status::Entering,
            node: Rc::new(node),
        });
    }
}

impl<'a, L: TrieLayout> TrieIterator<L> for TrieDBNodeIterator<'a, L> {
    fn seek(&mut self, key: &[u8]) -> Result<(), TrieHash<L>, CError<L>> {
        self.trail.clear();
        self.key_nibbles.clear();
        let root_node = self.db.root_data()?;
        self.seek(&root_node, NibbleSlice::new(key.as_ref()))
    }
}

impl<'a, L: TrieLayout> Iterator for TrieDBNodeIterator<'a, L> {
    type Item = Result<(NibbleVec, Rc<OwnedNode>), TrieHash<L>, CError<L>>;

    fn next(&mut self) -> Option<Self::Item> {
        enum IterStep<'b, O, E> {
            YieldNode,
            Continue,
            PopTrail,
            Descend(Result<Cow<'b, DBValue>, O, E>),
        }
        loop {
            let iter_step = {
                let b = self.trail.last()?;

                match (b.status.clone(), b.node.as_ref()) {
                    (Status::Entering, _) => IterStep::YieldNode,
                    (Status::Exiting, n) => {
                        match *n {
                            OwnedNode::Empty | OwnedNode::Leaf(_, _) => {},
                            OwnedNode::Extension(ref n, _) =>
                                self.key_nibbles.drop_lasts(n.len()),
                            OwnedNode::Branch(_) => { self.key_nibbles.pop(); },
                            OwnedNode::NibbledBranch(ref n, _) =>
                                self.key_nibbles.drop_lasts(n.len() + 1),
                        }
                        IterStep::PopTrail
                    },
                    (Status::At, &OwnedNode::Extension(ref partial, ref d)) => {
                        self.key_nibbles.append(partial);
                        IterStep::Descend::<TrieHash<L>, CError<L>>(
                            self.db.get_raw_or_lookup(&*d, self.key_nibbles.as_prefix())
                        )
                    },
                    (Status::At, &OwnedNode::Branch(_)) => {
                        self.key_nibbles.push(0);
                        IterStep::Continue
                    },
                    (Status::At, &OwnedNode::NibbledBranch(ref partial, _)) => {
                        self.key_nibbles.append(partial);
                        self.key_nibbles.push(0);
                        IterStep::Continue
                    },
                    (Status::AtChild(i), &OwnedNode::Branch(ref branch))
                    | (Status::AtChild(i), &OwnedNode::NibbledBranch(_, ref branch)) => {
                        if let Some(child) = branch.index(i) {
                            self.key_nibbles.pop();
                            self.key_nibbles.push(i as u8);
                            IterStep::Descend::<TrieHash<L>, CError<L>>(
                                self.db.get_raw_or_lookup(child, self.key_nibbles.as_prefix())
                            )
                        } else {
                            IterStep::Continue
                        }
                    },
                    _ => panic!(
                        "Crumb::increment and TrieDBNodeIterator are implemented so that the above \
                        arms are the only possible states"
                    ),
                }
            };

            match iter_step {
                IterStep::YieldNode => {
                    let crumb = self.trail.last_mut()
                        .expect(
                            "method would have exited at top of previous block if trial were empty;\
                            trial could not have been modified within the block since it was immutably borrowed;\
                            qed"
                        );
                    crumb.increment();
                    return Some(Ok((self.key_nibbles.clone(), crumb.node.clone())));
                },
                IterStep::PopTrail => {
                    self.trail.pop()
                        .expect(
                            "method would have exited at top of previous block if trial were empty;\
                            trial could not have been modified within the block since it was immutably borrowed;\
                            qed"
                        );
                    self.trail.last_mut()?
                        .increment();
                },
                IterStep::Descend::<TrieHash<L>, CError<L>>(next) => {
                    let node_result = next.and_then(|encoded|
                        L::Codec::decode(encoded.as_ref())
                            .map(Into::<OwnedNode>::into)
                            .map_err(|err| {
                                let node_hash = L::Hash::hash(encoded.as_ref());
                                Box::new(TrieError::DecoderError(node_hash, err))
                            })
                    );
                    match node_result {
                        Ok(node) => self.descend_into_node(node),
                        Err(err) => return Some(Err(err)),
                    }
                },
                IterStep::Continue => {
                    self.trail.last_mut()
                        .expect(
                            "method would have exited at top of previous block if trial were empty;\
                            trial could not have been modified within the block since it was immutably borrowed;\
                            qed"
                        )
                        .increment();
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::DBValue;
    use hash_db::Hasher;
    use keccak_hasher::KeccakHasher;
    use reference_trie::{
        RefTrieDB, RefTrieDBMut,
        TrieMut, TrieIterator, TrieDBNodeIterator, NibbleSlice, NibbleVec, node::OwnedNode,
    };
    use reference_trie::{RefTrieDBNoExt, RefTrieDBMutNoExt};

    type MemoryDB = memory_db::MemoryDB<KeccakHasher, memory_db::PrefixedKey<KeccakHasher>, DBValue>;

    fn build_trie_db_with_extension(pairs: &[(Vec<u8>, Vec<u8>)])
        -> (MemoryDB, <KeccakHasher as Hasher>::Out)
    {
        let mut memdb = MemoryDB::default();
        let mut root = Default::default();
        {
            let mut t = RefTrieDBMut::new(&mut memdb, &mut root);
            for (x, y) in pairs.iter() {
                t.insert(x, y).unwrap();
            }
        }
        (memdb, root)
    }

    fn build_trie_db_without_extension(pairs: &[(Vec<u8>, Vec<u8>)])
        -> (MemoryDB, <KeccakHasher as Hasher>::Out)
    {
        let mut memdb = MemoryDB::default();
        let mut root = Default::default();
        {
            let mut t = RefTrieDBMutNoExt::new(&mut memdb, &mut root);
            for (x, y) in pairs.iter() {
                t.insert(x, y).unwrap();
            }
        }
        (memdb, root)
    }

    fn nibble_vec<T: AsRef<[u8]>>(bytes: T, len: usize) -> NibbleVec {
        let slice = NibbleSlice::new(bytes.as_ref());

        let mut v = NibbleVec::new();
        for i in 0..len {
            v.push(slice.at(i));
        }
        v
    }

    #[test]
    fn iterator_works_with_extension() {
        let pairs = vec![
            (hex!("01").to_vec(), b"aaaa".to_vec()),
            (hex!("0123").to_vec(), b"bbbb".to_vec()),
            (hex!("02").to_vec(), b"cccc".to_vec()),
        ];

        let (memdb, root) = build_trie_db_with_extension(&pairs);
        let trie = RefTrieDB::new(&memdb, &root).unwrap();
        let mut iter = TrieDBNodeIterator::new(&trie).unwrap();

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!(""), 0));
                match node.as_ref() {
                    OwnedNode::Extension(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!("00"), 1)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("00"), 1));
                match node.as_ref() {
                    OwnedNode::Branch(_) => {},
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("01"), 2));
                match node.as_ref() {
                    OwnedNode::Branch(_) => {},
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("0120"), 3));
                match node.as_ref() {
                    OwnedNode::Leaf(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!("30"), 1)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("02"), 2));
                match node.as_ref() {
                    OwnedNode::Leaf(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!(""), 0)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        assert!(iter.next().is_none());
    }


    #[test]
    fn iterator_works_without_extension() {
        let pairs = vec![
            (hex!("01").to_vec(), b"aaaa".to_vec()),
            (hex!("0123").to_vec(), b"bbbb".to_vec()),
            (hex!("02").to_vec(), b"cccc".to_vec()),
        ];

        let (memdb, root) = build_trie_db_without_extension(&pairs);
        let trie = RefTrieDBNoExt::new(&memdb, &root).unwrap();
        let mut iter = TrieDBNodeIterator::new(&trie).unwrap();

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!(""), 0));
                match node.as_ref() {
                    OwnedNode::NibbledBranch(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!("00"), 1)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("01"), 2));
                match node.as_ref() {
                    OwnedNode::NibbledBranch(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!(""), 0)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("0120"), 3));
                match node.as_ref() {
                    OwnedNode::Leaf(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!("30"), 1)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!("02"), 2));
                match node.as_ref() {
                    OwnedNode::Leaf(partial, _) =>
                        assert_eq!(*partial, nibble_vec(hex!(""), 0)),
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        assert!(iter.next().is_none());
    }

    #[test]
    fn iterator_over_empty_works() {
        let (memdb, root) = build_trie_db_with_extension(&[]);
        let trie = RefTrieDB::new(&memdb, &root).unwrap();
        let mut iter = TrieDBNodeIterator::new(&trie).unwrap();

        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!(""), 0));
                match node.as_ref() {
                    OwnedNode::Empty => {},
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        assert!(iter.next().is_none());
    }

    #[test]
    fn seek_works_with_extension() {
        let pairs = vec![
            (hex!("01").to_vec(), b"aaaa".to_vec()),
            (hex!("0123").to_vec(), b"bbbb".to_vec()),
            (hex!("02").to_vec(), b"cccc".to_vec()),
        ];

        let (memdb, root) = build_trie_db_with_extension(&pairs);
        let trie = RefTrieDB::new(&memdb, &root).unwrap();
        let mut iter = TrieDBNodeIterator::new(&trie).unwrap();

        TrieIterator::seek(&mut iter, &hex!("")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!(""), 0)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("00")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!("01"), 2)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("01")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!("01"), 2)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("02")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!("02"), 2)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("03")[..]).unwrap();
        assert!(iter.next().is_none());
    }


    #[test]
    fn seek_works_without_extension() {
        let pairs = vec![
            (hex!("01").to_vec(), b"aaaa".to_vec()),
            (hex!("0123").to_vec(), b"bbbb".to_vec()),
            (hex!("02").to_vec(), b"cccc".to_vec()),
        ];

        let (memdb, root) = build_trie_db_without_extension(&pairs);
        let trie = RefTrieDBNoExt::new(&memdb, &root).unwrap();
        let mut iter = TrieDBNodeIterator::new(&trie).unwrap();

        TrieIterator::seek(&mut iter, &hex!("")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!(""), 0)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("00")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!("01"), 2)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("01")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!("01"), 2)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("02")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, _))) =>
                assert_eq!(prefix, nibble_vec(hex!("02"), 2)),
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("03")[..]).unwrap();
        assert!(iter.next().is_none());
    }

    #[test]
    fn seek_over_empty_works() {
        let (memdb, root) = build_trie_db_with_extension(&[]);
        let trie = RefTrieDB::new(&memdb, &root).unwrap();
        let mut iter = TrieDBNodeIterator::new(&trie).unwrap();

        TrieIterator::seek(&mut iter, &hex!("")[..]).unwrap();
        match iter.next() {
            Some(Ok((prefix, node))) => {
                assert_eq!(prefix, nibble_vec(hex!(""), 0));
                match node.as_ref() {
                    OwnedNode::Empty => {},
                    _ => panic!("unexpected node"),
                }
            }
            _ => panic!("unexpected item"),
        }

        TrieIterator::seek(&mut iter, &hex!("00")[..]).unwrap();
        assert!(iter.next().is_none());
    }
}

