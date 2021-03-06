// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use tezos_encoding::encoding::{Encoding, SchemaType};
use wireshark_epan_adapter::dissector::{Tree, TreeLeaf};
use bytes::Buf;
use chrono::NaiveDateTime;
use std::ops::Range;
use failure::Fail;
use bit_vec::BitVec;
use crypto::hash::HashType;
use crate::range_tool::intersect;

pub trait HasBodyRange {
    fn body(&self) -> Range<usize>;
}

#[derive(Debug, Fail)]
pub enum DecodingError {
    #[fail(display = "Not enough bytes")]
    NotEnoughData,
    #[fail(display = "Tag size not supported")]
    TagSizeNotSupported,
    #[fail(display = "Tag not found")]
    TagNotFound,
    #[fail(display = "Unexpected option value")]
    UnexpectedOptionDiscriminant,
    #[fail(display = "Path tag should be 0x00 or 0x0f or 0xf0")]
    BadPathTag,
}

#[derive(Debug)]
pub struct ChunkedData<'a, C>
where
    C: HasBodyRange,
{
    data: &'a [u8],
    chunks: &'a [C],
}

#[derive(Clone, Debug)]
pub struct ChunkedDataOffset {
    pub data_offset: usize,
    pub chunks_offset: usize,
}

impl ChunkedDataOffset {
    pub fn following(&self, length: usize) -> Range<usize> {
        self.data_offset..(self.data_offset + length)
    }
}

impl<'a, C> ChunkedData<'a, C>
where
    C: HasBodyRange,
{
    pub fn new(data: &'a [u8], chunks: &'a [C]) -> Self {
        ChunkedData { data, chunks }
    }

    fn limit(&self, offset: &ChunkedDataOffset, limit: usize) -> Result<Self, DecodingError> {
        let r = |i| -> Range<usize> {
            self.chunks
                .get(offset.chunks_offset + i)
                .map(|info: &C| {
                    let range = info.body();
                    let o = offset.data_offset;
                    let l = self.data.len();
                    usize::max(o, range.start)..usize::min(range.end, l)
                })
                .unwrap_or(0..0)
        };
        let mut limit = limit;
        let mut i = 0;
        let end = loop {
            if limit <= r(i).len() {
                break r(i).start + limit;
            } else if r(i).len() == 0 {
                return Err(DecodingError::NotEnoughData);
            } else {
                limit -= r(i).len();
                i += 1;
            }
        };
        Ok(ChunkedData {
            data: &self.data[..end],
            chunks: self.chunks,
        })
    }

    // try to cut `length` bytes, update offset
    // TODO: simplify it
    fn cut<F, T>(
        &self,
        offset: &mut ChunkedDataOffset,
        length: usize,
        f: F,
    ) -> Result<T, DecodingError>
    where
        F: FnOnce(&mut dyn Buf) -> T,
    {
        let range = self.chunks[offset.chunks_offset].body();
        assert!(
            range.contains(&offset.data_offset) || (offset.data_offset == range.end && length == 0)
        );
        let remaining = offset.data_offset..usize::min(range.end, self.data.len());
        if remaining.len() >= length {
            let end = remaining.start + length;
            offset.data_offset += length;
            Ok(f(&mut &self.data[remaining.start..end]))
        } else {
            let mut v = Vec::with_capacity(length);
            offset.data_offset += remaining.len();
            let mut length = length - remaining.len();
            v.extend_from_slice(&self.data[remaining]);
            loop {
                offset.chunks_offset += 1;
                if self.chunks.len() == offset.chunks_offset {
                    if length == 0 {
                        break;
                    } else {
                        return Err(DecodingError::NotEnoughData);
                    }
                } else {
                    let range = self.chunks[offset.chunks_offset].body();
                    let remaining = range.start..usize::min(range.end, self.data.len());
                    if remaining.len() >= length {
                        offset.data_offset =
                            self.chunks[offset.chunks_offset].body().start + length;
                        if length > 0 {
                            let end = remaining.start + length;
                            length = 0;
                            v.extend_from_slice(&self.data[remaining.start..end]);
                        }
                        break;
                    } else {
                        offset.data_offset += remaining.len();
                        length -= remaining.len();
                        v.extend_from_slice(&self.data[remaining]);
                    }
                }
            }
            assert_eq!(length, 0);
            Ok(f(&mut v.as_slice()))
        }
    }

    fn empty(&self, offset: &ChunkedDataOffset) -> bool {
        self.available(offset) == 0
    }

    fn available(&self, offset: &ChunkedDataOffset) -> usize {
        let end = usize::min(
            self.chunks[offset.chunks_offset].body().end,
            self.data.len(),
        );
        let available = end - offset.data_offset;
        // if it is the first message it always goes in the single chunk
        if offset.chunks_offset != 0 && self.chunks.len() - 1 > offset.chunks_offset {
            self.chunks[(offset.chunks_offset + 1)..]
                .iter()
                .fold(available, |a, c| {
                    if self.data.len() >= c.body().end {
                        a + c.body().len()
                    } else if self.data.len() > c.body().start {
                        a + (self.data.len() - c.body().start)
                    } else {
                        a
                    }
                })
        } else {
            available
        }
    }

    pub fn read_z(&self, offset: &mut ChunkedDataOffset) -> Result<String, DecodingError> {
        // read first byte
        let byte = self.cut(offset, 1, |b| b.get_u8())?;
        let negative = byte & (1 << 6) != 0;
        if byte <= 0x3F {
            let mut num = i32::from(byte);
            if negative {
                num *= -1;
            }
            Ok(format!("{:x}", num))
        } else {
            let mut bits = BitVec::new();
            for bit_idx in 0..6 {
                bits.push(byte & (1 << bit_idx) != 0);
            }

            let mut has_next_byte = true;
            while has_next_byte {
                let byte = self.cut(offset, 1, |b| b.get_u8())?;
                for bit_idx in 0..7 {
                    bits.push(byte & (1 << bit_idx) != 0)
                }

                has_next_byte = byte & (1 << 7) != 0;
            }

            let bytes = to_byte_vec(&trim_left(&reverse(&bits)));

            let mut str_num = bytes
                .iter()
                .enumerate()
                .map(|(idx, b)| match idx {
                    0 => format!("{:x}", *b),
                    _ => format!("{:02x}", *b),
                })
                .fold(String::new(), |mut str_num, val| {
                    str_num.push_str(&val);
                    str_num
                });
            if negative {
                str_num = String::from("-") + str_num.as_str();
            }

            Ok(str_num)
        }
    }

    pub fn read_mutez(&self, offset: &mut ChunkedDataOffset) -> Result<String, DecodingError> {
        let mut bits = BitVec::new();

        let mut has_next_byte = true;
        while has_next_byte {
            let byte = self.cut(offset, 1, |b| b.get_u8())?;
            for bit_idx in 0..7 {
                bits.push(byte & (1 << bit_idx) != 0)
            }

            has_next_byte = byte & (1 << 7) != 0;
        }

        let bytes = to_byte_vec(&trim_left(&reverse(&bits)));

        let str_num = bytes
            .iter()
            .enumerate()
            .map(|(idx, b)| match idx {
                0 => format!("{:x}", *b),
                _ => format!("{:02x}", *b),
            })
            .fold(String::new(), |mut str_num, val| {
                str_num.push_str(&val);
                str_num
            });

        Ok(str_num)
    }

    pub fn read_path(
        &self,
        offset: &mut ChunkedDataOffset,
        v: &mut Vec<String>,
    ) -> Result<(), DecodingError> {
        match self.cut(offset, 1, |b| b.get_u8())? {
            0x00 => Ok(()),
            0xf0 => {
                self.read_path(offset, v)?;
                let l = HashType::OperationListListHash.size();
                let hash = self.cut(offset, l, |b| hex::encode(b.bytes()))?;
                v.push(format!("left: {}", hash));
                Ok(())
            },
            0x0f => {
                let l = HashType::OperationListListHash.size();
                let hash = self.cut(offset, l, |b| hex::encode(b.bytes()))?;
                self.read_path(offset, v)?;
                v.push(format!("right: {}", hash));
                Ok(())
            },
            _ => Err(DecodingError::BadPathTag),
        }
    }

    pub fn show(
        &self,
        offset: &mut ChunkedDataOffset,
        encoding: &Encoding,
        space: &Range<usize>,
        base: &str,
        node: &mut Tree,
    ) -> Result<(), DecodingError> {
        match encoding {
            &Encoding::Unit => (),
            &Encoding::Int8 => {
                let item = offset.following(1);
                let value = self.cut(offset, item.len(), |b| b.get_i8())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value as _));
            },
            &Encoding::Uint8 => {
                let item = offset.following(1);
                let value = self.cut(offset, item.len(), |b| b.get_u8())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value as _));
            },
            &Encoding::Int16 => {
                let item = offset.following(2);
                let value = self.cut(offset, item.len(), |b| b.get_i16())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value as _));
            },
            &Encoding::Uint16 => {
                let item = offset.following(2);
                let value = self.cut(offset, item.len(), |b| b.get_u16())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value as _));
            },
            &Encoding::Int31 | &Encoding::Int32 => {
                let item = offset.following(4);
                let value = self.cut(offset, item.len(), |b| b.get_i32())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value as _));
            },
            &Encoding::Uint32 => {
                let item = offset.following(4);
                let value = self.cut(offset, item.len(), |b| b.get_u32())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value.into()));
            },
            &Encoding::Int64 => {
                let item = offset.following(8);
                let value = self.cut(offset, item.len(), |b| b.get_i64())?;
                node.add(base, intersect(space, item), TreeLeaf::dec(value as _));
            },
            &Encoding::RangedInt => unimplemented!(),
            &Encoding::Z => {
                let mut item = offset.following(0);
                let value = self.read_z(offset)?;
                item.end = offset.data_offset;
                node.add(base, intersect(space, item), TreeLeaf::Display(value));
            },
            &Encoding::Mutez => {
                let mut item = offset.following(0);
                let value = self.read_mutez(offset)?;
                item.end = offset.data_offset;
                node.add(base, intersect(space, item), TreeLeaf::Display(value));
            },
            &Encoding::Float => {
                let item = offset.following(8);
                let value = self.cut(offset, item.len(), |b| b.get_f64())?;
                node.add(base, intersect(space, item), TreeLeaf::float(value as _));
            },
            &Encoding::RangedFloat => unimplemented!(),
            &Encoding::Bool => {
                let item = offset.following(1);
                let value = self.cut(offset, item.len(), |d| d.get_u8() == 0xff)?;
                node.add(base, intersect(space, item), TreeLeaf::Display(value));
            },
            &Encoding::String => {
                let mut item = offset.following(4);
                let length = self.cut(offset, item.len(), |b| b.get_u32())? as usize;
                let f = |b: &mut dyn Buf| String::from_utf8((b.bytes()).to_owned()).ok();
                let string = self.cut(offset, length, f)?;
                item.end = offset.data_offset;
                if let Some(s) = string {
                    node.add(base, intersect(space, item), TreeLeaf::Display(s));
                }
            },
            &Encoding::Bytes => {
                let item = offset.following(self.available(offset));
                let string = self.cut(offset, item.len(), |d| hex::encode(d.bytes()))?;
                node.add(base, intersect(space, item), TreeLeaf::Display(string));
            },
            &Encoding::Tags(ref tag_size, ref tag_map) => {
                let id = match tag_size {
                    &1 => self.cut(offset, 1, |b| b.get_u8())? as u16,
                    &2 => self.cut(offset, 2, |b| b.get_u16())?,
                    _ => return Err(DecodingError::TagSizeNotSupported),
                };
                if let Some(tag) = tag_map.find_by_id(id) {
                    let encoding = tag.get_encoding();
                    let mut temp_offset = offset.clone();
                    let size = self.estimate_size(&mut temp_offset, encoding)?;
                    let item = offset.following(size);
                    let range = intersect(space, item);
                    let mut sub_node = node.add(base, range, TreeLeaf::nothing()).subtree();
                    let variant = tag.get_variant();
                    self.show(offset, encoding, space, variant, &mut sub_node)?;
                } else {
                    return Err(DecodingError::TagNotFound);
                }
            },
            &Encoding::List(ref encoding) => {
                if let &Encoding::Uint8 = encoding.as_ref() {
                    self.show(offset, &Encoding::Bytes, space, base, node)?;
                } else {
                    while !self.empty(offset) {
                        self.show(offset, encoding, space, base, node)?;
                    }
                }
            },
            &Encoding::Enum => self.show(offset, &Encoding::Uint32, space, base, node)?,
            &Encoding::Option(ref encoding) | &Encoding::OptionalField(ref encoding) => {
                match self.cut(offset, 1, |b| b.get_u8())? {
                    0 => (),
                    1 => self.show(offset, encoding, space, base, node)?,
                    _ => return Err(DecodingError::UnexpectedOptionDiscriminant),
                }
            },
            &Encoding::Obj(ref fields) => {
                let mut temp_offset = offset.clone();
                let size = self.estimate_size(&mut temp_offset, &Encoding::Obj(fields.clone()))?;
                let item = offset.following(size);
                let range = intersect(space, item);
                let mut sub_node = node.add(base, range, TreeLeaf::nothing()).subtree();
                for field in fields {
                    if field.get_name() == "operation_hashes_path" {
                        let mut item = offset.following(0);
                        let mut path = Vec::new();
                        self.read_path(offset, &mut path)?;
                        item.end = offset.data_offset;
                        let range = intersect(space, item);
                        let mut p = sub_node
                            .add(field.get_name(), range, TreeLeaf::nothing())
                            .subtree();
                        for component in path.into_iter().rev() {
                            p.add("path_component", 0..0, TreeLeaf::Display(component));
                        }
                    } else {
                        self.show(
                            offset,
                            field.get_encoding(),
                            space,
                            field.get_name(),
                            &mut sub_node,
                        )?;
                    }
                }
            },
            &Encoding::Tup(ref encodings) => {
                let mut temp_offset = offset.clone();
                let size =
                    self.estimate_size(&mut temp_offset, &Encoding::Tup(encodings.clone()))?;
                let item = offset.following(size);
                let range = intersect(space, item);
                let mut sub_node = node.add(base, range, TreeLeaf::nothing()).subtree();
                for (i, encoding) in encodings.iter().enumerate() {
                    let n = format!("{}", i);
                    self.show(offset, encoding, space, &n, &mut sub_node)?;
                }
            },
            &Encoding::Dynamic(ref encoding) => {
                // TODO: use item, highlight the length
                let item = offset.following(4);
                let length = self.cut(offset, item.len(), |b| b.get_u32())? as usize;
                if length <= self.available(offset) {
                    self.limit(offset, length)?
                        .show(offset, encoding, space, base, node)?;
                } else {
                    // report error
                }
            },
            &Encoding::Sized(ref size, ref encoding) => {
                self.limit(offset, size.clone())?
                    .show(offset, encoding, space, base, node)?;
            },
            &Encoding::Greedy(ref encoding) => {
                self.show(offset, encoding, space, base, node)?;
            },
            &Encoding::Hash(ref hash_type) => {
                let item = offset.following(hash_type.size());
                let string = self.cut(offset, item.len(), |d| hex::encode(d.bytes()))?;
                node.add(base, intersect(space, item), TreeLeaf::Display(string));
            },
            &Encoding::Split(ref f) => {
                self.show(offset, &f(SchemaType::Binary), space, base, node)?;
            },
            &Encoding::Timestamp => {
                let item = offset.following(8);
                let value = self.cut(offset, item.len(), |b| b.get_i64())?;
                let time = NaiveDateTime::from_timestamp(value, 0);
                node.add(base, intersect(space, item), TreeLeaf::Display(time));
            },
            &Encoding::Lazy(ref _f) => {
                panic!("should not happen");
            },
        };
        Ok(())
    }

    // TODO: it is double work, optimize it out
    // we should store decoded data and show it only when whole node is collected
    pub fn estimate_size(
        &self,
        offset: &mut ChunkedDataOffset,
        encoding: &Encoding,
    ) -> Result<usize, DecodingError> {
        match encoding {
            &Encoding::Unit => Ok(0),
            &Encoding::Int8 | &Encoding::Uint8 => self.cut(offset, 1, |a| a.bytes().len()),
            &Encoding::Int16 | &Encoding::Uint16 => self.cut(offset, 2, |a| a.bytes().len()),
            &Encoding::Int31 | &Encoding::Int32 | &Encoding::Uint32 => {
                self.cut(offset, 4, |a| a.bytes().len())
            },
            &Encoding::Int64 => self.cut(offset, 8, |a| a.bytes().len()),
            &Encoding::RangedInt => unimplemented!(),
            &Encoding::Z => {
                let start = offset.data_offset;
                let _ = self.read_z(offset)?;
                Ok(offset.data_offset - start)
            },
            &Encoding::Mutez => {
                let start = offset.data_offset;
                let _ = self.read_mutez(offset)?;
                Ok(offset.data_offset - start)
            },
            &Encoding::Float => self.cut(offset, 8, |a| a.bytes().len()),
            &Encoding::RangedFloat => unimplemented!(),
            &Encoding::Bool => self.cut(offset, 1, |a| a.bytes().len()),
            &Encoding::String => {
                let l = self.cut(offset, 4, |b| b.get_u32())? as usize;
                self.cut(offset, l, |a| a.bytes().len() + 4)
            },
            &Encoding::Bytes => {
                let l = self.available(offset);
                self.cut(offset, l, |a| a.bytes().len())
            },
            &Encoding::Tags(ref tag_size, ref tag_map) => {
                let id = match tag_size {
                    &1 => self.cut(offset, 1, |b| b.get_u8())? as u16,
                    &2 => self.cut(offset, 2, |b| b.get_u16())?,
                    _ => {
                        log::warn!("unsupported tag size");
                        return Err(DecodingError::TagSizeNotSupported);
                    },
                };
                if let Some(tag) = tag_map.find_by_id(id) {
                    self.estimate_size(offset, tag.get_encoding())
                        .map(|s| s + tag_size.clone())
                } else {
                    Err(DecodingError::TagNotFound)
                }
            },
            &Encoding::List(_) => {
                let l = self.available(offset);
                self.cut(offset, l, |a| a.bytes().len())
            },
            &Encoding::Enum => self.estimate_size(offset, &Encoding::Uint32),
            &Encoding::Option(ref encoding) | &Encoding::OptionalField(ref encoding) => {
                match self.cut(offset, 1, |b| b.get_u8())? {
                    0 => Ok(1),
                    1 => self.estimate_size(offset, encoding).map(|s| s + 1),
                    _ => Err(DecodingError::UnexpectedOptionDiscriminant),
                }
            },
            &Encoding::Tup(ref encodings) => encodings
                .iter()
                .map(|e| self.estimate_size(offset, e))
                .try_fold(0, |sum, size_at| size_at.map(|s| s + sum)),
            &Encoding::Obj(ref fields) => fields
                .iter()
                .map(|f| {
                    if f.get_name() == "operation_hashes_path" {
                        let start = offset.data_offset;
                        self.read_path(offset, &mut Vec::new())?;
                        Ok(offset.data_offset - start)
                    } else {
                        self.estimate_size(offset, f.get_encoding())
                    }
                })
                .try_fold(0, |sum, size_at_field| size_at_field.map(|s| s + sum)),
            &Encoding::Dynamic(_) => {
                let l = self.cut(offset, 4, |b| b.get_u32())? as usize;
                self.cut(offset, l, |a| a.bytes().len() + 4)
            },
            &Encoding::Sized(ref size, _) => self.cut(offset, size.clone(), |a| a.bytes().len()),
            &Encoding::Greedy(_) => {
                let l = self.available(offset);
                self.cut(offset, l, |a| a.bytes().len())
            },
            &Encoding::Hash(ref hash_type) => {
                self.cut(offset, hash_type.size(), |a| a.bytes().len())
            },
            &Encoding::Timestamp => self.cut(offset, 8, |a| a.bytes().len()),
            &Encoding::Split(ref f) => self.estimate_size(offset, &f(SchemaType::Binary)),
            &Encoding::Lazy(ref _f) => panic!("should not happen"),
        }
    }
}

fn reverse(s: &BitVec) -> BitVec {
    let mut reversed = BitVec::new();
    for bit in s.iter().rev() {
        reversed.push(bit)
    }
    reversed
}

fn trim_left(s: &BitVec) -> BitVec {
    let mut trimmed: BitVec = BitVec::new();

    let mut notrim = false;
    for bit in s.iter() {
        if bit {
            trimmed.push(bit);
            notrim = true;
        } else if notrim {
            trimmed.push(bit);
        }
    }
    trimmed
}

fn to_byte_vec(s: &BitVec) -> Vec<u8> {
    let mut bytes = vec![];
    let mut byte = 0;
    let mut offset = 0;
    for (idx_bit, bit) in s.iter().rev().enumerate() {
        let idx_byte = (idx_bit % 8) as u8;
        if bit {
            byte |= 1 << idx_byte;
        } else {
            byte &= !(1 << idx_byte);
        }
        if idx_byte == 7 {
            bytes.push(byte);
            byte = 0;
        }
        offset = idx_byte;
    }
    if offset != 7 {
        bytes.push(byte);
    }
    bytes.reverse();
    bytes
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use super::{ChunkedData, ChunkedDataOffset, HasBodyRange};

    impl HasBodyRange for Range<usize> {
        fn body(&self) -> Range<usize> {
            self.clone()
        }
    }

    fn with_test_data<F>(f: F)
    where
        F: FnOnce(ChunkedData<Range<usize>>),
    {
        let data = {
            let mut v = Vec::new();
            v.resize(128, 'x' as u8);
            v
        };
        let (data, chunks, _) = [
            ['a' as u8; 12].as_ref(),
            ['b' as u8; 16].as_ref(),
            ['c' as u8; 24].as_ref(),
            ['d' as u8; 8].as_ref(),
        ]
        .iter()
        .fold(
            (data, Vec::new(), 0),
            |(mut data, mut chunks, mut start), c| {
                let end = start + c.len();
                data[start..end].clone_from_slice(*c);
                chunks.push(start..end);
                start = end + 4;
                (data, chunks, start)
            },
        );

        f(ChunkedData {
            data: data.as_ref(),
            chunks: chunks.as_ref(),
        })
    }

    #[test]
    fn simple_cut() {
        let mut offset = ChunkedDataOffset {
            chunks_offset: 0,
            data_offset: 0,
        };

        with_test_data(|data| {
            let cut = data
                .cut(&mut offset, 25, |b| {
                    String::from_utf8(b.to_bytes().to_vec()).unwrap()
                })
                .unwrap();
            assert_eq!(cut, "aaaaaaaaaaaabbbbbbbbbbbbb");
            let cut = data
                .cut(&mut offset, 35, |b| {
                    String::from_utf8(b.to_bytes().to_vec()).unwrap()
                })
                .unwrap();
            assert_eq!(cut, "bbbccccccccccccccccccccccccdddddddd");
        });
    }
}
