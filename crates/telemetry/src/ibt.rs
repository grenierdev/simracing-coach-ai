//! iRacing telemetry files (.ibt): reading and writing.
//!
//! An .ibt contains: `irsdk_header`, `irsdk_diskSubHeader`, the session YAML,
//! the variable table, then `recordCount` rows of `bufLen` bytes starting at
//! `varBuf[0].bufOffset`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use memmap2::Mmap;
use thiserror::Error;

use crate::irsdk::{
    DISK_SUB_HEADER_LEN, DiskSubHeader, FormatError, HEADER_LEN, Header, VAR_HEADER_LEN, VarBuf,
    VarHeader, VarTable, VarType, session_info_yaml,
};
use crate::sample::{ChannelMap, Sample};
use crate::session::SessionInfo;

#[derive(Debug, Error)]
pub enum IbtError {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("format: {0}")]
    Format(#[from] FormatError),
    #[error("required channels missing: {0:?}")]
    MissingChannels(Vec<&'static str>),
}

enum Data {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl Data {
    fn bytes(&self) -> &[u8] {
        match self {
            Data::Mapped(m) => m,
            Data::Owned(v) => v,
        }
    }
}

pub struct IbtFile {
    data: Data,
    pub header: Header,
    pub disk: DiskSubHeader,
    pub vars: VarTable,
    pub session_yaml: String,
    pub session: SessionInfo,
    channels: ChannelMap,
    record_count: usize,
}

impl IbtFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IbtError> {
        let file = File::open(path)?;
        // SAFETY: the file is opened read-only; if it were truncated during
        // reading, access would crash. Acceptable for a finished telemetry
        // file.
        let mmap = unsafe { Mmap::map(&file)? };
        Self::from_data(Data::Mapped(mmap))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, IbtError> {
        Self::from_data(Data::Owned(bytes))
    }

    fn from_data(data: Data) -> Result<Self, IbtError> {
        let b = data.bytes();
        let header = Header::parse(b)?;
        let disk = DiskSubHeader::parse(b)?;
        let vars = VarTable::parse(b, &header)?;
        let missing = ChannelMap::missing_required(&vars);
        if !missing.is_empty() {
            return Err(IbtError::MissingChannels(missing));
        }
        let session_yaml = session_info_yaml(b, &header).unwrap_or_default();
        let session = SessionInfo::parse(&session_yaml);
        let channels = ChannelMap::new(&vars);
        let start = header.var_buf[0].buf_offset.max(0) as usize;
        let row_len = header.buf_len as usize;
        // recordCount may be wrong if iRacing crashed: rely on the actual size.
        let available = b
            .len()
            .saturating_sub(start)
            .checked_div(row_len)
            .unwrap_or(0);
        let record_count = if disk.record_count > 0 {
            (disk.record_count as usize).min(available)
        } else {
            available
        };
        Ok(IbtFile {
            data,
            header,
            disk,
            vars,
            session_yaml,
            session,
            channels,
            record_count,
        })
    }

    pub fn len(&self) -> usize {
        self.record_count
    }

    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    pub fn tick_rate(&self) -> f64 {
        if self.header.tick_rate > 0 {
            self.header.tick_rate as f64
        } else {
            60.0
        }
    }

    pub fn row(&self, i: usize) -> Option<&[u8]> {
        if i >= self.record_count {
            return None;
        }
        let start = self.header.var_buf[0].buf_offset as usize + i * self.header.buf_len as usize;
        self.data
            .bytes()
            .get(start..start + self.header.buf_len as usize)
    }

    pub fn sample(&self, i: usize) -> Option<Sample> {
        self.row(i).map(|r| self.channels.decode(r))
    }

    pub fn samples(&self) -> impl Iterator<Item = Sample> + '_ {
        (0..self.record_count).filter_map(|i| self.sample(i))
    }
}

/// Channel value for [`IbtWriter`].
#[derive(Debug, Clone, Copy)]
pub enum Value {
    Bool(bool),
    Int(i32),
    Float(f32),
    Double(f64),
}

/// Writes an .ibt. Used by tests, the synthetic generator and, later, live
/// session recording.
pub struct IbtWriter {
    vars: Vec<VarHeader>,
    row_len: usize,
    rows: Vec<u8>,
    session_yaml: String,
    tick_rate: i32,
}

impl IbtWriter {
    /// `channels`: (name, type, unit).
    pub fn new(channels: &[(&str, VarType, &str)], session_yaml: &str, tick_rate: i32) -> Self {
        let mut offset: usize = 0;
        let vars = channels
            .iter()
            .map(|(name, ty, unit)| {
                // Natural alignment of the type.
                let size = ty.size();
                offset = offset.div_ceil(size) * size;
                let v = VarHeader {
                    ty: *ty,
                    offset,
                    count: 1,
                    count_as_time: false,
                    name: name.to_string(),
                    desc: String::new(),
                    unit: unit.to_string(),
                };
                offset += size;
                v
            })
            .collect();
        IbtWriter {
            vars,
            row_len: offset.div_ceil(4) * 4,
            rows: Vec::new(),
            session_yaml: session_yaml.to_string(),
            tick_rate,
        }
    }

    /// Adds a row. `values` in the order of the declared channels.
    pub fn push(&mut self, values: &[Value]) {
        assert_eq!(values.len(), self.vars.len(), "number of values");
        let base = self.rows.len();
        self.rows.resize(base + self.row_len, 0);
        let row = &mut self.rows[base..];
        for (v, val) in self.vars.iter().zip(values) {
            let o = v.offset;
            match (v.ty, *val) {
                (VarType::Bool, Value::Bool(x)) => row[o] = x as u8,
                (VarType::Int | VarType::BitField, Value::Int(x)) => {
                    row[o..o + 4].copy_from_slice(&x.to_le_bytes())
                }
                (VarType::Float, Value::Float(x)) => {
                    row[o..o + 4].copy_from_slice(&x.to_le_bytes())
                }
                (VarType::Double, Value::Double(x)) => {
                    row[o..o + 8].copy_from_slice(&x.to_le_bytes())
                }
                (ty, val) => panic!("incompatible type for {}: {ty:?} / {val:?}", v.name),
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        // iRacing encodes the YAML as ISO-8859-1.
        let yaml: Vec<u8> = self
            .session_yaml
            .chars()
            .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
            .collect();
        let yaml = yaml.as_slice();
        let session_info_offset = HEADER_LEN + DISK_SUB_HEADER_LEN;
        let session_info_len = yaml.len() + 1;
        let var_header_offset = (session_info_offset + session_info_len).div_ceil(16) * 16;
        let data_offset = var_header_offset + self.vars.len() * VAR_HEADER_LEN;
        let record_count = self.rows.len() / self.row_len.max(1);

        let mut out = vec![0u8; data_offset];
        let mut var_buf = [VarBuf::default(); 4];
        var_buf[0] = VarBuf {
            tick_count: record_count as i32,
            buf_offset: data_offset as i32,
        };
        Header {
            ver: 2,
            status: 1,
            tick_rate: self.tick_rate,
            session_info_update: 0,
            session_info_len: session_info_len as i32,
            session_info_offset: session_info_offset as i32,
            num_vars: self.vars.len() as i32,
            var_header_offset: var_header_offset as i32,
            num_buf: 1,
            buf_len: self.row_len as i32,
            var_buf,
        }
        .write(&mut out);
        DiskSubHeader {
            start_date: 0,
            start_time: 0.0,
            end_time: record_count as f64 / self.tick_rate as f64,
            lap_count: 0,
            record_count: record_count as i32,
        }
        .write(&mut out);
        out[session_info_offset..session_info_offset + yaml.len()].copy_from_slice(yaml);
        for (i, v) in self.vars.iter().enumerate() {
            let o = var_header_offset + i * VAR_HEADER_LEN;
            v.write(&mut out[o..o + VAR_HEADER_LEN]);
        }
        out.extend_from_slice(&self.rows);
        out
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut f = File::create(path)?;
        f.write_all(&self.to_bytes())
    }
}
