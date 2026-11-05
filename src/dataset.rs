//! Parsers for PTP datasets (DeviceInfo, ObjectInfo, StorageInfo, DevicePropDesc).

use anyhow::{Result, bail};
use std::fmt;

use crate::ptp::dtc;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            bail!(
                "dataset truncated: need {n} bytes at offset {}, have {}",
                self.pos,
                self.remaining()
            );
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    /// PTP string: u8 number of UTF-16 code units (including NUL), then UTF-16LE.
    pub fn string(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        let raw = self.bytes(n * 2)?;
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }

    pub fn array_u16(&mut self) -> Result<Vec<u16>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.u16()).collect()
    }

    pub fn array_u32(&mut self) -> Result<Vec<u32>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.u32()).collect()
    }

    /// Read a value of the given PTP data type.
    pub fn value(&mut self, datatype: u16) -> Result<PropValue> {
        Ok(match datatype {
            dtc::INT8 => PropValue::Int(self.u8()? as i8 as i64),
            dtc::UINT8 => PropValue::UInt(self.u8()? as u64),
            dtc::INT16 => PropValue::Int(self.u16()? as i16 as i64),
            dtc::UINT16 => PropValue::UInt(self.u16()? as u64),
            dtc::INT32 => PropValue::Int(self.u32()? as i32 as i64),
            dtc::UINT32 => PropValue::UInt(self.u32()? as u64),
            dtc::INT64 => PropValue::Int(self.u64()? as i64),
            dtc::UINT64 => PropValue::UInt(self.u64()?),
            dtc::INT128 | dtc::UINT128 => PropValue::Raw(self.bytes(16)?.to_vec()),
            dtc::STR => PropValue::Str(self.string()?),
            t if t & dtc::ARRAY_MASK != 0 => {
                let elem = t & !dtc::ARRAY_MASK;
                let n = self.u32()? as usize;
                let mut v = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    v.push(self.value(elem)?);
                }
                PropValue::Array(v)
            }
            t => bail!("unsupported PTP data type 0x{t:04x}"),
        })
    }
}

/// Encode a value for SetDevicePropValue.
pub fn encode_value(datatype: u16, v: &PropValue) -> Result<Vec<u8>> {
    let as_i128 = |v: &PropValue| -> Result<i128> {
        match v {
            PropValue::Int(i) => Ok(*i as i128),
            PropValue::UInt(u) => Ok(*u as i128),
            _ => bail!("expected an integer value"),
        }
    };
    Ok(match datatype {
        dtc::INT8 | dtc::UINT8 => vec![as_i128(v)? as u8],
        dtc::INT16 | dtc::UINT16 => (as_i128(v)? as u16).to_le_bytes().to_vec(),
        dtc::INT32 | dtc::UINT32 => (as_i128(v)? as u32).to_le_bytes().to_vec(),
        dtc::INT64 | dtc::UINT64 => (as_i128(v)? as u64).to_le_bytes().to_vec(),
        dtc::STR => {
            let PropValue::Str(s) = v else {
                bail!("expected a string value")
            };
            let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            if units.len() > 255 {
                bail!("string too long");
            }
            let mut out = vec![units.len() as u8];
            out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            out
        }
        t => bail!("cannot encode data type 0x{t:04x}"),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    Int(i64),
    UInt(u64),
    Str(String),
    Array(Vec<PropValue>),
    Raw(Vec<u8>),
}

impl PropValue {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            PropValue::Int(i) => Some(*i as u64),
            PropValue::UInt(u) => Some(*u),
            _ => None,
        }
    }
}

impl fmt::Display for PropValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropValue::Int(i) => write!(f, "{i}"),
            PropValue::UInt(u) => write!(f, "{u} (0x{u:x})"),
            PropValue::Str(s) => write!(f, "{s:?}"),
            PropValue::Array(a) => {
                write!(f, "[")?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            PropValue::Raw(b) => {
                for x in b {
                    write!(f, "{x:02x}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub standard_version: u16,
    pub vendor_extension_id: u32,
    pub vendor_extension_version: u16,
    pub vendor_extension_desc: String,
    pub functional_mode: u16,
    pub operations: Vec<u16>,
    pub events: Vec<u16>,
    pub properties: Vec<u16>,
    pub capture_formats: Vec<u16>,
    pub image_formats: Vec<u16>,
    pub manufacturer: String,
    pub model: String,
    pub device_version: String,
    pub serial_number: String,
}

impl DeviceInfo {
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        Ok(DeviceInfo {
            standard_version: r.u16()?,
            vendor_extension_id: r.u32()?,
            vendor_extension_version: r.u16()?,
            vendor_extension_desc: r.string()?,
            functional_mode: r.u16()?,
            operations: r.array_u16()?,
            events: r.array_u16()?,
            properties: r.array_u16()?,
            capture_formats: r.array_u16()?,
            image_formats: r.array_u16()?,
            manufacturer: r.string()?,
            model: r.string()?,
            device_version: r.string()?,
            serial_number: r.string()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct StorageInfo {
    pub storage_type: u16,
    pub filesystem_type: u16,
    pub access: u16,
    pub max_capacity: u64,
    pub free_bytes: u64,
    pub free_images: u32,
    pub description: String,
    pub label: String,
}

impl StorageInfo {
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        Ok(StorageInfo {
            storage_type: r.u16()?,
            filesystem_type: r.u16()?,
            access: r.u16()?,
            max_capacity: r.u64()?,
            free_bytes: r.u64()?,
            free_images: r.u32()?,
            description: r.string()?,
            label: r.string()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub storage_id: u32,
    pub format: u16,
    pub protection: u16,
    pub compressed_size: u32,
    pub thumb_format: u16,
    pub thumb_compressed_size: u32,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub image_bit_depth: u32,
    pub parent: u32,
    pub association_type: u16,
    pub association_desc: u32,
    pub sequence_number: u32,
    pub filename: String,
    pub capture_date: String,
    pub modification_date: String,
    pub keywords: String,
}

impl ObjectInfo {
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let mut o = ObjectInfo {
            storage_id: r.u32()?,
            format: r.u16()?,
            protection: r.u16()?,
            compressed_size: r.u32()?,
            thumb_format: r.u16()?,
            thumb_compressed_size: r.u32()?,
            thumb_width: r.u32()?,
            thumb_height: r.u32()?,
            image_width: r.u32()?,
            image_height: r.u32()?,
            image_bit_depth: r.u32()?,
            parent: r.u32()?,
            association_type: r.u16()?,
            association_desc: r.u32()?,
            sequence_number: r.u32()?,
            filename: r.string()?,
            capture_date: String::new(),
            modification_date: String::new(),
            keywords: String::new(),
        };
        // Some cameras truncate the trailing strings; treat them as optional.
        if r.remaining() > 0 {
            o.capture_date = r.string().unwrap_or_default();
        }
        if r.remaining() > 0 {
            o.modification_date = r.string().unwrap_or_default();
        }
        if r.remaining() > 0 {
            o.keywords = r.string().unwrap_or_default();
        }
        Ok(o)
    }
}

#[derive(Debug, Clone)]
pub enum PropForm {
    None,
    Range {
        min: PropValue,
        max: PropValue,
        step: PropValue,
    },
    Enum(Vec<PropValue>),
}

#[derive(Debug, Clone)]
pub struct PropDesc {
    pub code: u16,
    pub datatype: u16,
    pub writable: bool,
    pub default: PropValue,
    pub current: PropValue,
    pub form: PropForm,
}

impl PropDesc {
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let code = r.u16()?;
        let datatype = r.u16()?;
        let writable = r.u8()? != 0;
        let default = r.value(datatype)?;
        let current = r.value(datatype)?;
        let form = if r.remaining() == 0 {
            PropForm::None
        } else {
            match r.u8()? {
                1 => PropForm::Range {
                    min: r.value(datatype)?,
                    max: r.value(datatype)?,
                    step: r.value(datatype)?,
                },
                2 => {
                    let n = r.u16()? as usize;
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(r.value(datatype)?);
                    }
                    PropForm::Enum(v)
                }
                _ => PropForm::None,
            }
        };
        Ok(PropDesc {
            code,
            datatype,
            writable,
            default,
            current,
            form,
        })
    }
}

/// Parse the Fuji events list returned for property 0xD212:
/// `u16 count` followed by `count × (u16 property code, u32 value)`.
pub fn parse_fuji_events(b: &[u8]) -> Result<Vec<(u16, u32)>> {
    let mut r = Reader::new(b);
    let n = r.u16()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push((r.u16()?, r.u32()?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ptp_str(s: &str) -> Vec<u8> {
        let u: Vec<u16> = s.encode_utf16().chain([0]).collect();
        let mut v = vec![u.len() as u8];
        v.extend(u.iter().flat_map(|x| x.to_le_bytes()));
        v
    }

    #[test]
    fn device_info() {
        let mut b = vec![];
        b.extend(100u16.to_le_bytes());
        b.extend(6u32.to_le_bytes());
        b.extend(100u16.to_le_bytes());
        b.extend(ptp_str("fujifilm.co.jp: 1.0;"));
        b.extend(0u16.to_le_bytes());
        b.extend(2u32.to_le_bytes());
        b.extend(0x1001u16.to_le_bytes());
        b.extend(0x1002u16.to_le_bytes());
        for _ in 0..4 {
            b.extend(0u32.to_le_bytes());
        }
        b.extend(ptp_str("FUJIFILM"));
        b.extend(ptp_str("X-T5"));
        b.extend(ptp_str("1.00"));
        b.extend(ptp_str("ABC"));
        let d = DeviceInfo::parse(&b).unwrap();
        assert_eq!(d.model, "X-T5");
        assert_eq!(d.operations, vec![0x1001, 0x1002]);
        assert_eq!(d.serial_number, "ABC");
    }

    #[test]
    fn prop_desc_enum() {
        let mut b = vec![];
        b.extend(0x5005u16.to_le_bytes());
        b.extend(dtc::UINT16.to_le_bytes());
        b.push(1);
        b.extend(2u16.to_le_bytes());
        b.extend(4u16.to_le_bytes());
        b.push(2);
        b.extend(2u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(4u16.to_le_bytes());
        let d = PropDesc::parse(&b).unwrap();
        assert!(d.writable);
        assert_eq!(d.current, PropValue::UInt(4));
        assert!(matches!(d.form, PropForm::Enum(ref v) if v.len() == 2));
    }

    #[test]
    fn fuji_events() {
        let mut b = vec![];
        b.extend(2u16.to_le_bytes());
        b.extend(0xd222u16.to_le_bytes());
        b.extend(12u32.to_le_bytes());
        b.extend(0x5001u16.to_le_bytes());
        b.extend(3u32.to_le_bytes());
        assert_eq!(
            parse_fuji_events(&b).unwrap(),
            vec![(0xd222, 12), (0x5001, 3)]
        );
    }

    #[test]
    fn string_roundtrip() {
        let enc = encode_value(dtc::STR, &PropValue::Str("hé".into())).unwrap();
        let mut r = Reader::new(&enc);
        assert_eq!(r.string().unwrap(), "hé");
    }
}
