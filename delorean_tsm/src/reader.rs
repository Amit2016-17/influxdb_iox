//! Types for reading and writing TSM files produced by InfluxDB >= 2.x

use super::encoders::*;
use super::TSMError;
use integer_encoding::VarInt;
use std::io::{BufRead, Seek, SeekFrom};
use std::u64;

/// `TSMIndexReader` allows you to read index data within a TSM file.
///
/// # Example
///
/// Iterating over the TSM index.
///
/// ```
/// # use delorean_tsm::reader::*;
/// # use libflate::gzip;
/// # use std::fs::File;
/// # use std::io::BufReader;
/// # use std::io::Cursor;
/// # use std::io::Read;
/// # let file = File::open("../tests/fixtures/000000000000005-000000002.tsm.gz");
/// # let mut decoder = gzip::Decoder::new(file.unwrap()).unwrap();
/// # let mut buf = Vec::new();
/// # decoder.read_to_end(&mut buf).unwrap();
/// # let data_len = buf.len();
/// # let r = Cursor::new(buf);
///
/// let reader = TSMIndexReader::try_new(BufReader::new(r), 4_222_248).unwrap();
///
/// // reader allows you to access each index entry, and each block for each
/// // entry in order.
/// for index_entry in reader {
///     match index_entry {
///         Ok(entry) => {
///             let key = entry.parse_key().unwrap();
///             println!(
///                 "bucket id is {:?}, measurement name is {:?}",
///                 entry.bucket_id(),
///                 key.measurement,
///             )
///         }
///         Err(e) => println!("got an error {:?}", e),
///     }
/// }
/// ```
///
#[derive(Debug)]
pub struct TSMIndexReader<R>
where
    R: BufRead + Seek,
{
    r: R,

    curr_offset: u64,
    end_offset: u64,

    curr: Option<IndexEntry>,
    next: Option<IndexEntry>,
}

impl<R> TSMIndexReader<R>
where
    R: BufRead + Seek,
{
    pub fn try_new(mut r: R, len: usize) -> Result<Self, TSMError> {
        // determine offset to index, which is held in last 8 bytes of file.
        r.seek(SeekFrom::End(-8))?;
        let mut buf: [u8; 8] = [0; 8];
        r.read_exact(&mut buf)?;

        let index_offset = u64::from_be_bytes(buf);
        r.seek(SeekFrom::Start(index_offset))?;

        Ok(Self {
            r,
            curr_offset: index_offset,
            end_offset: len as u64 - 8,
            curr: None,
            next: None,
        })
    }

    /// next_index_entry will return either the next index entry in a TSM file's
    /// index or will return an error. `next_index_entry` updates the offset on
    /// the Index, but it's the caller's responsibility to stop reading entries
    /// when the index has been exhausted.
    fn next_index_entry(&mut self) -> Result<IndexEntry, TSMError> {
        // read length of series key
        let mut buf: [u8; 2] = [0; 2];
        self.r.read_exact(&mut buf)?;
        self.curr_offset += 2;
        let key_len = u16::from_be_bytes(buf);

        // read the series key itself
        let mut key_bytes = vec![0; key_len as usize]; // TODO(edd): re-use this
        self.r.read_exact(key_bytes.as_mut_slice())?;
        self.curr_offset += key_len as u64;

        // read the block type
        self.r.read_exact(&mut buf[..1])?;
        self.curr_offset += 1;
        let block_type = buf[0];

        // read how many blocks there are for this entry.
        self.r.read_exact(&mut buf)?;
        self.curr_offset += 2;
        let count = u16::from_be_bytes(buf);

        Ok(IndexEntry {
            key: key_bytes,
            block_type,
            count,
            curr_block: 1,
            block: self.next_block_entry()?,
        })
    }

    /// next_block_entry will return the next block entry within an index entry.
    /// It is the caller's responsibility to stop reading block entries when
    /// they have all been read for an index entry.
    fn next_block_entry(&mut self) -> Result<Block, TSMError> {
        // read min time on block entry
        let mut buf: [u8; 8] = [0; 8];
        self.r.read_exact(&mut buf[..])?;
        self.curr_offset += 8;
        let min_time = i64::from_be_bytes(buf);

        // read max time on block entry
        self.r.read_exact(&mut buf[..])?;
        self.curr_offset += 8;
        let max_time = i64::from_be_bytes(buf);

        // read block data offset
        self.r.read_exact(&mut buf[..])?;
        self.curr_offset += 8;
        let offset = u64::from_be_bytes(buf);

        // read block size
        self.r.read_exact(&mut buf[..4])?;
        self.curr_offset += 4;
        let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);

        Ok(Block {
            min_time,
            max_time,
            offset,
            size,
        })
    }
}

impl<R: BufRead + Seek> Iterator for TSMIndexReader<R> {
    type Item = Result<IndexEntry, TSMError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr_offset == self.end_offset {
            // end of entries
            return None;
        }

        match &self.curr {
            Some(curr) => {
                if curr.curr_block < curr.count {
                    // there are more block entries for this index entry. Read
                    // the next block entry.
                    let mut next = curr.clone();
                    match self.next_block_entry() {
                        Ok(block) => next.block = block,
                        Err(e) => return Some(Err(e)),
                    }
                    next.curr_block += 1;
                    self.next = Some(next);
                } else {
                    // no more block entries. Move onto the next entry.
                    match self.next_index_entry() {
                        Ok(entry) => self.next = Some(entry),
                        Err(e) => return Some(Err(e)),
                    }
                }
            }
            None => match self.next_index_entry() {
                Ok(entry) => self.next = Some(entry),
                Err(e) => return Some(Err(e)),
            },
        }

        self.curr = self.next.clone();
        Some(Ok(self.curr.clone().unwrap()))
    }
}

/// `IndexEntry` provides lazy accessors for components of the entry.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    key: Vec<u8>,

    pub block_type: u8,
    pub count: u16,
    pub block: Block,
    curr_block: u16,
}

impl IndexEntry {
    /// Get the organization ID that this entry belongs to.
    pub fn org_id(&self) -> InfluxID {
        IndexEntry::extract_id_from_slice(&self.key[..8])
    }

    /// Get the bucket ID that this entry belongs to.
    pub fn bucket_id(&self) -> InfluxID {
        IndexEntry::extract_id_from_slice(&self.key[8..16])
    }

    fn extract_id_from_slice(data: &[u8]) -> InfluxID {
        let mut buf: [u8; 8] = [0; 8];
        buf.copy_from_slice(&data[..8]);
        InfluxID::from_be_bytes(buf)
    }

    pub fn parse_key(&self) -> Result<ParsedTSMKey, TSMError> {
        parse_tsm_key(self.key.to_vec())
    }
}

#[derive(Clone, Debug)]
pub struct ParsedTSMKey {
    pub measurement: String,
    pub tagset: Vec<(String, String)>,
    pub field_key: String,
}

/// parse_tsm_key parses from the series key the measurement, field key and tag
/// set.
///
/// It does not provide access to the org and bucket ids on the key, these can
/// be accessed via org_id() and bucket_id() respectively.
///
/// TODO: handle escapes in the series key for , = and \t
///
fn parse_tsm_key(mut key: Vec<u8>) -> Result<ParsedTSMKey, TSMError> {
    // skip over org id, bucket id, comma, null byte (measurement) and =
    // The next n-1 bytes are the measurement name, where the nᵗʰ byte is a `,`.
    key = key.drain(8 + 8 + 1 + 1 + 1..).collect::<Vec<u8>>();
    let mut i = 0;
    // TODO(edd): can we make this work with take_while?
    while i != key.len() {
        if key[i] == b',' {
            break;
        }
        i += 1;
    }

    let mut rem_key = key.drain(i..).collect::<Vec<u8>>();
    let measurement = String::from_utf8(key).map_err(|e| TSMError {
        description: e.to_string(),
    })?;

    let mut tagset = Vec::<(String, String)>::with_capacity(10);
    let mut reading_key = true;
    let mut key = String::with_capacity(100);
    let mut value = String::with_capacity(100);

    // skip the comma separating measurement tag
    for byte in rem_key.drain(1..) {
        match byte {
            44 => {
                // ,
                reading_key = true;
                tagset.push((key, value));
                key = String::with_capacity(250);
                value = String::with_capacity(250);
            }
            61 => {
                // =
                reading_key = false;
            }
            _ => {
                if reading_key {
                    key.push(byte as char);
                } else {
                    value.push(byte as char);
                }
            }
        }
    }

    // fields are stored on the series keys in TSM indexes as follows:
    //
    // <field_key><4-byte delimiter><field_key>
    //
    // so we can trim the parsed value.
    let field_trim_length = (value.len() - 4) / 2;
    let (field, _) = value.split_at(field_trim_length);
    Ok(ParsedTSMKey {
        measurement,
        tagset,
        field_key: field.to_string(),
    })
}

pub const F64_BLOCKTYPE_MARKER: u8 = 0;
pub const I64_BLOCKTYPE_MARKER: u8 = 1;
pub const BOOL_BLOCKTYPE_MARKER: u8 = 2;
pub const STRING_BLOCKTYPE_MARKER: u8 = 3;
pub const U64_BLOCKTYPE_MARKER: u8 = 4;

/// `TSMBlockReader` allows you to read and decode TSM blocks from within a TSM
/// file.
///
#[derive(Debug)]
pub struct TSMBlockReader<R>
where
    R: BufRead + Seek,
{
    r: R,
}

impl<R> TSMBlockReader<R>
where
    R: BufRead + Seek,
{
    pub fn new(r: R) -> Self {
        Self { r }
    }

    /// decode_block decodes a block whose location is described by the provided
    /// `Block`.
    ///
    /// The components of the returned `BlockData` are guaranteed to have
    /// identical lengths.
    pub fn decode_block(&mut self, block: &Block) -> Result<BlockData, TSMError> {
        self.r.seek(SeekFrom::Start(block.offset))?;

        let mut data: Vec<u8> = vec![0; block.size as usize];
        self.r.read_exact(&mut data)?;

        // TODO(edd): skip 32-bit CRC checksum at beginning of block for now
        let mut idx = 4;

        // determine the block type
        let block_type = data[idx];
        idx += 1;

        // first decode the timestamp block.
        let mut ts: Vec<i64> = Vec::with_capacity(MAX_BLOCK_VALUES); // 1000 is the max block size
        let (len, n) = u64::decode_var(&data[idx..]); // size of timestamp block
        idx += n;
        timestamp::decode(&data[idx..idx + (len as usize)], &mut ts).map_err(|e| TSMError {
            description: e.to_string(),
        })?;
        idx += len as usize;

        match block_type {
            F64_BLOCKTYPE_MARKER => {
                // values will be same length as time-stamps.
                let mut values: Vec<f64> = Vec::with_capacity(ts.len());
                float::decode_influxdb(&data[idx..], &mut values).map_err(|e| TSMError {
                    description: e.to_string(),
                })?;

                Ok(BlockData::Float { ts, values })
            }
            I64_BLOCKTYPE_MARKER => {
                // values will be same length as time-stamps.
                let mut values: Vec<i64> = Vec::with_capacity(ts.len());
                integer::decode(&data[idx..], &mut values).map_err(|e| TSMError {
                    description: e.to_string(),
                })?;

                Ok(BlockData::Integer { ts, values })
            }
            BOOL_BLOCKTYPE_MARKER => Err(TSMError {
                description: String::from("bool block type unsupported"),
            }),
            STRING_BLOCKTYPE_MARKER => Err(TSMError {
                description: String::from("string block type unsupported"),
            }),
            U64_BLOCKTYPE_MARKER => Err(TSMError {
                description: String::from("unsigned integer block type unsupported"),
            }),
            _ => Err(TSMError {
                description: format!("unsupported block type {:?}", block_type),
            }),
        }
    }
}

/// `Block` holds information about location and time range of a block of data.
#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub struct Block {
    pub min_time: i64,
    pub max_time: i64,
    pub offset: u64,
    pub size: u32,
}

// MAX_BLOCK_VALUES is the maximum number of values a TSM block can store.
const MAX_BLOCK_VALUES: usize = 1000;

/// `BlockData` describes the various types of block data that can be held within
/// a TSM file.
#[derive(Debug)]
pub enum BlockData {
    Float { ts: Vec<i64>, values: Vec<f64> },
    Integer { ts: Vec<i64>, values: Vec<i64> },
    Bool { ts: Vec<i64>, values: Vec<bool> },
    Str { ts: Vec<i64>, values: Vec<String> },
    Unsigned { ts: Vec<i64>, values: Vec<u64> },
}

impl BlockData {
    pub fn is_empty(&self) -> bool {
        match &self {
            BlockData::Float { ts, values: _ } => ts.is_empty(),
            BlockData::Integer { ts, values: _ } => ts.is_empty(),
            BlockData::Bool { ts, values: _ } => ts.is_empty(),
            BlockData::Str { ts, values: _ } => ts.is_empty(),
            BlockData::Unsigned { ts, values: _ } => ts.is_empty(),
        }
    }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
/// `InfluxID` represents an InfluxDB ID used in InfluxDB 2.x to represent
/// organization and bucket identifiers.
pub struct InfluxID(u64);

#[allow(dead_code)]
impl InfluxID {
    fn new_str(s: &str) -> Result<InfluxID, TSMError> {
        let v = u64::from_str_radix(s, 16).map_err(|e| TSMError {
            description: e.to_string(),
        })?;
        Ok(InfluxID(v))
    }

    fn from_be_bytes(bytes: [u8; 8]) -> InfluxID {
        InfluxID(u64::from_be_bytes(bytes))
    }
}

impl std::fmt::Display for InfluxID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libflate::gzip;
    use std::fs::File;
    use std::i64;
    use std::io::BufReader;
    use std::io::Cursor;
    use std::io::Read;

    #[test]
    fn read_tsm_index() {
        let file = File::open("../tests/fixtures/000000000000005-000000002.tsm.gz");
        let mut decoder = gzip::Decoder::new(file.unwrap()).unwrap();
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).unwrap();

        let reader = TSMIndexReader::try_new(BufReader::new(Cursor::new(buf)), 4_222_248).unwrap();

        assert_eq!(reader.curr_offset, 3_893_272);
        assert_eq!(reader.count(), 2159)
    }

    #[test]
    fn read_tsm_block() {
        let file = File::open("../tests/fixtures/000000000000005-000000002.tsm.gz");
        let mut decoder = gzip::Decoder::new(file.unwrap()).unwrap();
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).unwrap();

        let reader = TSMIndexReader::try_new(BufReader::new(Cursor::new(buf)), 4_222_248).unwrap();

        let mut got_blocks: u64 = 0;
        let mut got_min_time = i64::MAX;
        let mut got_max_time = i64::MIN;

        // every block in the fixture file is for the 05c19117091a1000 org and
        // 05c19117091a1001 bucket.
        let org_id = InfluxID::new_str("05c19117091a1000").unwrap();
        let bucket_id = InfluxID::new_str("05c19117091a1001").unwrap();

        for index_entry in reader {
            match index_entry {
                Ok(entry) => {
                    // TODO(edd): this is surely not the right way. I should be
                    // returning mutable references from the iterator.
                    let e = entry.clone();
                    got_blocks += e.count as u64;

                    if entry.block.min_time < got_min_time {
                        got_min_time = e.block.min_time;
                    }

                    if entry.block.max_time > got_max_time {
                        got_max_time = e.block.max_time;
                    }

                    assert_eq!(e.org_id(), org_id);
                    assert_eq!(e.bucket_id(), bucket_id);

                    assert!(
                        e.parse_key().is_ok(),
                        format!(
                            "failed to parse key name for {:}",
                            String::from_utf8_lossy(entry.key.as_slice())
                        )
                    );
                }
                Err(e) => panic!("{:?} {:?}", e, got_blocks),
            }
        }

        assert_eq!(got_blocks, 2159); // 2,159 blocks in the file
        assert_eq!(got_min_time, 1_590_585_404_546_128_000); // earliest time is 2020-05-27T13:16:44.546128Z
        assert_eq!(got_max_time, 1_590_597_378_379_824_000); // latest time is 2020-05-27T16:36:18.379824Z
    }

    #[test]
    fn decode_tsm_blocks() {
        let file = File::open("../tests/fixtures/000000000000005-000000002.tsm.gz");
        let mut decoder = gzip::Decoder::new(file.unwrap()).unwrap();
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).unwrap();
        let r = Cursor::new(buf);

        let mut block_reader = TSMBlockReader::new(BufReader::new(r));

        let block_defs = vec![
            super::Block {
                min_time: 1590585530000000000,
                max_time: 1590590600000000000,
                offset: 5339,
                size: 153,
            },
            super::Block {
                min_time: 1590585520000000000,
                max_time: 1590590600000000000,
                offset: 190770,
                size: 30,
            },
        ];

        let mut blocks = vec![];
        for def in block_defs {
            blocks.push(block_reader.decode_block(&def).unwrap());
        }

        for block in blocks {
            // The first integer block in the value should have 509 values in it.
            match block {
                BlockData::Float { ts, values } => {
                    assert_eq!(ts.len(), 507);
                    assert_eq!(values.len(), 507);
                }
                BlockData::Integer { ts, values } => {
                    assert_eq!(ts.len(), 509);
                    assert_eq!(values.len(), 509);
                }
                BlockData::Bool { ts: _, values: _ } => {
                    panic!("should not have decoded bool block")
                }
                BlockData::Str { ts: _, values: _ } => panic!("should not have decoded str block"),
                BlockData::Unsigned { ts: _, values: _ } => {
                    panic!("should not have decoded unsigned block")
                }
            }
        }
    }

    #[test]
    fn influx_id() {
        let id = InfluxID::new_str("20aa9b0").unwrap();
        assert_eq!(id, InfluxID(34_253_232));
        assert_eq!(format!("{}", id), "00000000020aa9b0");
    }

    #[test]
    fn parse_tsm_key() {
        //<org_id bucket_id>,\x00=http_api_request_duration_seconds,handler=platform,method=POST,path=/api/v2/setup,status=2XX,user_agent=Firefox,\xff=sum#!~#sum
        let buf = vec![
            "05C19117091A100005C19117091A10012C003D68747470",
            "5F6170695F726571756573745F6475726174696F6E5F73",
            "65636F6E64732C68616E646C65723D706C6174666F726D",
            "2C6D6574686F643D504F53542C706174683D2F6170692F",
            "76322F73657475702C7374617475733D3258582C757365",
            "725F6167656E743D46697265666F782CFF3D73756D2321",
            "7E2373756D",
        ]
        .join("");
        let tsm_key = hex::decode(buf).unwrap();

        let parsed_key = super::parse_tsm_key(tsm_key).unwrap();
        assert_eq!(
            parsed_key.measurement,
            String::from("http_api_request_duration_seconds")
        );

        let exp_tagset = vec![
            (String::from("handler"), String::from("platform")),
            (String::from("method"), String::from("POST")),
            (String::from("path"), String::from("/api/v2/setup")),
            (String::from("status"), String::from("2XX")),
            (String::from("user_agent"), String::from("Firefox")),
        ];
        assert_eq!(parsed_key.tagset, exp_tagset);
        assert_eq!(parsed_key.field_key, String::from("sum"));
    }

    // This test scans over the entire tsm contents and
    // ensures no errors are returned from the reader.
    fn walk_index_and_check_for_errors(tsm_gz_path: &str) {
        let file = File::open(tsm_gz_path);
        let mut decoder = gzip::Decoder::new(file.unwrap()).unwrap();
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).unwrap();
        let data_len = buf.len();

        let mut index_reader =
            TSMIndexReader::try_new(BufReader::new(Cursor::new(&buf)), data_len).unwrap();
        let mut blocks = Vec::new();

        for res in &mut index_reader {
            let entry = res.unwrap();
            let key = entry.parse_key().unwrap();
            assert!(!key.measurement.is_empty());

            let block_type = entry.block_type;
            if block_type == BOOL_BLOCKTYPE_MARKER {
                eprintln!("Note: ignoring bool block, not implemented");
            } else if block_type == STRING_BLOCKTYPE_MARKER {
                eprintln!("Note: ignoring string block, not implemented");
            } else if block_type == U64_BLOCKTYPE_MARKER {
                eprintln!("Note: ignoring bool block, not implemented");
            } else {
                blocks.push(entry.block);
            }
        }

        let mut block_reader = TSMBlockReader::new(Cursor::new(&buf));
        for block in blocks {
            block_reader
                .decode_block(&block)
                .expect("error decoding block data");
        }
    }

    #[test]
    fn check_tsm_cpu_usage() {
        walk_index_and_check_for_errors("../tests/fixtures/cpu_usage.tsm.gz");
    }

    #[test]
    fn check_tsm_000000000000005_000000002() {
        walk_index_and_check_for_errors("../tests/fixtures/000000000000005-000000002.tsm.gz");
    }
}
