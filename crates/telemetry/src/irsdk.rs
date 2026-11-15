//! Format binaire du SDK iRacing, commun à la mémoire partagée et aux fichiers .ibt.
//!
//! Référence : `irsdk_defines.h` du SDK officiel (structures `irsdk_header`,
//! `irsdk_diskSubHeader`, `irsdk_varHeader`, `irsdk_varBuf`). Toutes les valeurs
//! sont little-endian, alignement sur 4 octets.
//!
//! ```text
//! irsdk_header (112 octets)
//!   0  ver                 i32
//!   4  status              i32   bit 0 = connecté
//!   8  tickRate            i32   60 en général
//!  12  sessionInfoUpdate   i32   incrémenté à chaque changement du YAML
//!  16  sessionInfoLen      i32
//!  20  sessionInfoOffset   i32
//!  24  numVars             i32
//!  28  varHeaderOffset     i32
//!  32  numBuf              i32   <= 4
//!  36  bufLen              i32   taille d'une ligne de données
//!  40  pad[2]              i32
//!  48  varBuf[4]           { tickCount i32, bufOffset i32, pad[2] i32 } = 16 octets chacun
//!
//! irsdk_diskSubHeader (.ibt uniquement, juste après le header, 32 octets)
//! 112  sessionStartDate    i64 (time_t)
//! 120  sessionStartTime    f64
//! 128  sessionEndTime      f64
//! 136  sessionLapCount     i32
//! 140  sessionRecordCount  i32
//!
//! irsdk_varHeader (144 octets)
//!   0  type        i32
//!   4  offset      i32   décalage dans la ligne de données
//!   8  count       i32   nombre d'éléments (tableaux)
//!  12  countAsTime u8 + pad[3]
//!  16  name[32]
//!  48  desc[64]
//! 112  unit[32]
//! ```

use std::collections::HashMap;

use thiserror::Error;

pub const HEADER_LEN: usize = 112;
pub const DISK_SUB_HEADER_LEN: usize = 32;
pub const VAR_HEADER_LEN: usize = 144;
pub const MAX_BUFS: usize = 4;
pub const STATUS_CONNECTED: i32 = 1;

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("données tronquées : {0}")]
    Truncated(&'static str),
    #[error("en-tête invalide : {0}")]
    Invalid(String),
}

pub(crate) fn read_i32(b: &[u8], off: usize) -> Result<i32, FormatError> {
    b.get(off..off + 4)
        .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
        .ok_or(FormatError::Truncated("i32"))
}

fn read_i64(b: &[u8], off: usize) -> Result<i64, FormatError> {
    b.get(off..off + 8)
        .map(|s| i64::from_le_bytes(s.try_into().unwrap()))
        .ok_or(FormatError::Truncated("i64"))
}

fn read_f64(b: &[u8], off: usize) -> Result<f64, FormatError> {
    b.get(off..off + 8)
        .map(|s| f64::from_le_bytes(s.try_into().unwrap()))
        .ok_or(FormatError::Truncated("f64"))
}

/// Chaîne C de taille fixe, encodée en ISO-8859-1 par iRacing.
fn read_cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    latin1_to_string(&b[..end])
}

/// iRacing écrit le YAML et les noms en ISO-8859-1 (Windows-1252 en pratique).
pub fn latin1_to_string(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VarBuf {
    pub tick_count: i32,
    pub buf_offset: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub ver: i32,
    pub status: i32,
    pub tick_rate: i32,
    pub session_info_update: i32,
    pub session_info_len: i32,
    pub session_info_offset: i32,
    pub num_vars: i32,
    pub var_header_offset: i32,
    pub num_buf: i32,
    pub buf_len: i32,
    pub var_buf: [VarBuf; MAX_BUFS],
}

impl Header {
    pub fn parse(b: &[u8]) -> Result<Self, FormatError> {
        if b.len() < HEADER_LEN {
            return Err(FormatError::Truncated("irsdk_header"));
        }
        let mut var_buf = [VarBuf::default(); MAX_BUFS];
        for (i, vb) in var_buf.iter_mut().enumerate() {
            let base = 48 + i * 16;
            vb.tick_count = read_i32(b, base)?;
            vb.buf_offset = read_i32(b, base + 4)?;
        }
        let h = Header {
            ver: read_i32(b, 0)?,
            status: read_i32(b, 4)?,
            tick_rate: read_i32(b, 8)?,
            session_info_update: read_i32(b, 12)?,
            session_info_len: read_i32(b, 16)?,
            session_info_offset: read_i32(b, 20)?,
            num_vars: read_i32(b, 24)?,
            var_header_offset: read_i32(b, 28)?,
            num_buf: read_i32(b, 32)?,
            buf_len: read_i32(b, 36)?,
            var_buf,
        };
        if h.num_vars < 0 || h.num_vars > 10_000 {
            return Err(FormatError::Invalid(format!("numVars = {}", h.num_vars)));
        }
        if h.num_buf < 0 || h.num_buf as usize > MAX_BUFS {
            return Err(FormatError::Invalid(format!("numBuf = {}", h.num_buf)));
        }
        if h.buf_len < 0 {
            return Err(FormatError::Invalid(format!("bufLen = {}", h.buf_len)));
        }
        Ok(h)
    }

    pub fn is_connected(&self) -> bool {
        self.status & STATUS_CONNECTED != 0
    }

    /// Tampon le plus récent (tickCount le plus élevé) parmi les `numBuf` tampons rotatifs.
    pub fn latest_buf(&self) -> VarBuf {
        self.var_buf[..self.num_buf.max(1) as usize]
            .iter()
            .copied()
            .max_by_key(|vb| vb.tick_count)
            .unwrap_or_default()
    }

    pub fn write(&self, out: &mut [u8]) {
        let put = |out: &mut [u8], off: usize, v: i32| {
            out[off..off + 4].copy_from_slice(&v.to_le_bytes())
        };
        put(out, 0, self.ver);
        put(out, 4, self.status);
        put(out, 8, self.tick_rate);
        put(out, 12, self.session_info_update);
        put(out, 16, self.session_info_len);
        put(out, 20, self.session_info_offset);
        put(out, 24, self.num_vars);
        put(out, 28, self.var_header_offset);
        put(out, 32, self.num_buf);
        put(out, 36, self.buf_len);
        for (i, vb) in self.var_buf.iter().enumerate() {
            put(out, 48 + i * 16, vb.tick_count);
            put(out, 48 + i * 16 + 4, vb.buf_offset);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiskSubHeader {
    pub start_date: i64,
    pub start_time: f64,
    pub end_time: f64,
    pub lap_count: i32,
    pub record_count: i32,
}

impl DiskSubHeader {
    pub fn parse(b: &[u8]) -> Result<Self, FormatError> {
        let o = HEADER_LEN;
        Ok(DiskSubHeader {
            start_date: read_i64(b, o)?,
            start_time: read_f64(b, o + 8)?,
            end_time: read_f64(b, o + 16)?,
            lap_count: read_i32(b, o + 24)?,
            record_count: read_i32(b, o + 28)?,
        })
    }

    pub fn write(&self, out: &mut [u8]) {
        let o = HEADER_LEN;
        out[o..o + 8].copy_from_slice(&self.start_date.to_le_bytes());
        out[o + 8..o + 16].copy_from_slice(&self.start_time.to_le_bytes());
        out[o + 16..o + 24].copy_from_slice(&self.end_time.to_le_bytes());
        out[o + 24..o + 28].copy_from_slice(&self.lap_count.to_le_bytes());
        out[o + 28..o + 32].copy_from_slice(&self.record_count.to_le_bytes());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    Char,
    Bool,
    Int,
    BitField,
    Float,
    Double,
}

impl VarType {
    pub fn from_i32(v: i32) -> Option<Self> {
        Some(match v {
            0 => VarType::Char,
            1 => VarType::Bool,
            2 => VarType::Int,
            3 => VarType::BitField,
            4 => VarType::Float,
            5 => VarType::Double,
            _ => return None,
        })
    }

    pub fn to_i32(self) -> i32 {
        match self {
            VarType::Char => 0,
            VarType::Bool => 1,
            VarType::Int => 2,
            VarType::BitField => 3,
            VarType::Float => 4,
            VarType::Double => 5,
        }
    }

    pub fn size(self) -> usize {
        match self {
            VarType::Char | VarType::Bool => 1,
            VarType::Int | VarType::BitField | VarType::Float => 4,
            VarType::Double => 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarHeader {
    pub ty: VarType,
    pub offset: usize,
    pub count: usize,
    pub count_as_time: bool,
    pub name: String,
    pub desc: String,
    pub unit: String,
}

impl VarHeader {
    pub fn parse(b: &[u8]) -> Result<Self, FormatError> {
        if b.len() < VAR_HEADER_LEN {
            return Err(FormatError::Truncated("irsdk_varHeader"));
        }
        let ty_raw = read_i32(b, 0)?;
        let ty = VarType::from_i32(ty_raw)
            .ok_or_else(|| FormatError::Invalid(format!("type de variable {ty_raw}")))?;
        let offset = read_i32(b, 4)?;
        let count = read_i32(b, 8)?;
        if offset < 0 || count < 0 {
            return Err(FormatError::Invalid("offset ou count négatif".into()));
        }
        Ok(VarHeader {
            ty,
            offset: offset as usize,
            count: count as usize,
            count_as_time: b[12] != 0,
            name: read_cstr(&b[16..48]),
            desc: read_cstr(&b[48..112]),
            unit: read_cstr(&b[112..144]),
        })
    }

    pub fn write(&self, out: &mut [u8]) {
        out[..VAR_HEADER_LEN].fill(0);
        out[0..4].copy_from_slice(&self.ty.to_i32().to_le_bytes());
        out[4..8].copy_from_slice(&(self.offset as i32).to_le_bytes());
        out[8..12].copy_from_slice(&(self.count as i32).to_le_bytes());
        out[12] = self.count_as_time as u8;
        let put_str = |out: &mut [u8], s: &str, max: usize| {
            let bytes = s.as_bytes();
            let n = bytes.len().min(max - 1);
            out[..n].copy_from_slice(&bytes[..n]);
        };
        put_str(&mut out[16..48], &self.name, 32);
        put_str(&mut out[48..112], &self.desc, 64);
        put_str(&mut out[112..144], &self.unit, 32);
    }
}

/// Table des variables, indexée par nom.
#[derive(Debug, Clone, Default)]
pub struct VarTable {
    pub vars: Vec<VarHeader>,
    by_name: HashMap<String, usize>,
}

impl VarTable {
    pub fn parse(b: &[u8], header: &Header) -> Result<Self, FormatError> {
        let mut vars = Vec::with_capacity(header.num_vars as usize);
        for i in 0..header.num_vars as usize {
            let start = header.var_header_offset as usize + i * VAR_HEADER_LEN;
            let slice = b
                .get(start..start + VAR_HEADER_LEN)
                .ok_or(FormatError::Truncated("varHeaders"))?;
            let v = VarHeader::parse(slice)?;
            if v.offset + v.ty.size() * v.count > header.buf_len as usize {
                return Err(FormatError::Invalid(format!(
                    "variable {} hors de la ligne de données",
                    v.name
                )));
            }
            vars.push(v);
        }
        Ok(Self::from_vars(vars))
    }

    pub fn from_vars(vars: Vec<VarHeader>) -> Self {
        let by_name = vars
            .iter()
            .enumerate()
            .map(|(i, v)| (v.name.clone(), i))
            .collect();
        VarTable { vars, by_name }
    }

    pub fn get(&self, name: &str) -> Option<&VarHeader> {
        self.by_name.get(name).map(|&i| &self.vars[i])
    }
}

/// Lit la valeur scalaire (premier élément) d'une variable, convertie en f64.
pub fn read_value(row: &[u8], var: &VarHeader) -> Option<f64> {
    let o = var.offset;
    let bytes = row.get(o..o + var.ty.size())?;
    Some(match var.ty {
        VarType::Char => bytes[0] as f64,
        VarType::Bool => (bytes[0] != 0) as i32 as f64,
        VarType::Int => i32::from_le_bytes(bytes.try_into().ok()?) as f64,
        VarType::BitField => u32::from_le_bytes(bytes.try_into().ok()?) as f64,
        VarType::Float => f32::from_le_bytes(bytes.try_into().ok()?) as f64,
        VarType::Double => f64::from_le_bytes(bytes.try_into().ok()?),
    })
}

/// Extrait la chaîne YAML des informations de session.
pub fn session_info_yaml(b: &[u8], header: &Header) -> Option<String> {
    let start = header.session_info_offset as usize;
    let len = header.session_info_len as usize;
    let raw = b.get(start..start + len)?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(decode_session_yaml(&raw[..end]))
}

/// Le YAML est en Windows-1252, sauf s'il déclare `Encoding: UTF8` dans `WeekendInfo`.
pub fn decode_session_yaml(raw: &[u8]) -> String {
    let head = &raw[..raw.len().min(200)];
    let utf8 = head.windows(14).any(|w| w == b"Encoding: UTF8");
    if utf8 {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        latin1_to_string(raw)
    }
}
