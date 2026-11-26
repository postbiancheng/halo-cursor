//! Minimal protobuf / Connect framing helpers (clean-room, Magpie-aligned).

#[derive(Default)]
pub struct Pb {
    parts: Vec<Vec<u8>>,
}

impl Pb {
    pub fn new() -> Self {
        Self::default()
    }

    fn push_bytes(mut self, bytes: &[u8]) -> Self {
        self.parts.push(bytes.to_vec());
        self
    }

    fn uv(mut self, mut value: u64) -> Self {
        let mut out = Vec::new();
        while value >= 0x80 {
            out.push(((value as u8) & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        self.parts.push(out);
        self
    }

    fn tag(self, num: u32, wire: u8) -> Self {
        self.uv(u64::from(num) * 8 + u64::from(wire))
    }

    pub fn varint(self, num: u32, value: u64) -> Self {
        self.tag(num, 0).uv(value)
    }

    pub fn bytes(self, num: u32, value: &[u8]) -> Self {
        self.tag(num, 2).uv(value.len() as u64).push_bytes(value)
    }

    pub fn str(self, num: u32, value: &str) -> Self {
        self.bytes(num, value.as_bytes())
    }

    pub fn double(self, num: u32, value: f64) -> Self {
        self.tag(num, 1).push_bytes(&value.to_le_bytes())
    }

    pub fn nested(self, num: u32, child: Pb) -> Self {
        let bytes = child.done();
        self.bytes(num, &bytes)
    }

    pub fn done(self) -> Vec<u8> {
        self.parts.into_iter().flatten().collect()
    }
}

#[derive(Clone)]
pub struct Field {
    pub num: u32,
    #[allow(dead_code)]
    pub wire: u8,
    pub n: u64,
    pub data: Vec<u8>,
}

pub fn fields(input: &[u8]) -> Vec<Field> {
    checked_fields(input).unwrap_or_default()
}

pub fn checked_fields_502(input: &[u8]) -> agent2api_wasm_sdk::Result<Vec<Field>> {
    checked_fields(input).map_err(|error| agent2api_wasm_sdk::Error::http(502, error))
}

pub fn checked_fields(input: &[u8]) -> Result<Vec<Field>, &'static str> {
    const MALFORMED: &str = "Cursor Protobuf 字段无效";
    let mut out = Vec::new();
    let mut i = 0usize;
    let uv = |buf: &[u8], i: &mut usize| -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        while *i < buf.len() {
            let c = buf[*i];
            *i += 1;
            if shift == 63 && c > 1 {
                return None;
            }
            value |= u64::from(c & 0x7f) << shift;
            if c < 0x80 {
                return Some(value);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
        None
    };
    while i < input.len() {
        let Some(key) = uv(input, &mut i) else {
            return Err(MALFORMED);
        };
        if !(1..=0x1fff_ffff).contains(&(key / 8)) {
            return Err(MALFORMED);
        }
        let num = (key / 8) as u32;
        let wire = (key % 8) as u8;
        let mut field = Field {
            num,
            wire,
            n: 0,
            data: Vec::new(),
        };
        match wire {
            0 => {
                let Some(v) = uv(input, &mut i) else {
                    return Err(MALFORMED);
                };
                field.n = v;
            }
            1 => {
                if i + 8 > input.len() {
                    return Err(MALFORMED);
                }
                field.data = input[i..i + 8].to_vec();
                field.n = u64::from_le_bytes(input[i..i + 8].try_into().unwrap_or([0; 8]));
                i += 8;
            }
            2 => {
                let Some(len) = uv(input, &mut i) else {
                    return Err(MALFORMED);
                };
                if len > (input.len() - i) as u64 {
                    return Err(MALFORMED);
                }
                let len = len as usize;
                field.data = input[i..i + len].to_vec();
                i += len;
            }
            5 => {
                if i + 4 > input.len() {
                    return Err(MALFORMED);
                }
                field.n = u32::from_le_bytes(input[i..i + 4].try_into().unwrap_or([0; 4])) as u64;
                i += 4;
            }
            _ => return Err(MALFORMED),
        }
        if out.len() >= 16_384 {
            return Err("Cursor Protobuf 字段数量超限");
        }
        out.push(field);
    }
    Ok(out)
}

pub fn field(fs: &[Field], num: u32) -> Option<&Field> {
    fs.iter().find(|f| f.num == num)
}

pub fn pb_str(fs: &[Field], num: u32) -> String {
    field(fs, num)
        .map(|f| String::from_utf8_lossy(&f.data).into_owned())
        .unwrap_or_default()
}

pub fn pb_num(fs: &[Field], num: u32) -> u64 {
    field(fs, num).map(|f| f.n).unwrap_or(0)
}

pub fn pb_value(value: &serde_json::Value) -> Vec<u8> {
    match value {
        serde_json::Value::Null => Pb::new().varint(1, 0).done(),
        serde_json::Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            Pb::new().double(2, f).done()
        }
        serde_json::Value::String(s) => Pb::new().str(3, s).done(),
        serde_json::Value::Bool(b) => Pb::new().varint(4, u64::from(*b)).done(),
        serde_json::Value::Object(map) => {
            let mut st = Pb::new();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            for key in keys {
                let entry = Pb::new().str(1, &key).bytes(2, &pb_value(&map[&key]));
                st = st.nested(1, entry);
            }
            Pb::new().nested(5, st).done()
        }
        serde_json::Value::Array(items) => {
            let mut list = Pb::new();
            for item in items {
                list = list.bytes(1, &pb_value(item));
            }
            Pb::new().nested(6, list).done()
        }
    }
}

pub fn pb_any(data: &[u8]) -> Option<serde_json::Value> {
    pb_any_at_depth(data, 0)
}

fn pb_any_at_depth(data: &[u8], depth: usize) -> Option<serde_json::Value> {
    use serde_json::Value;
    if depth >= 64 {
        return None;
    }
    for f in checked_fields(data).ok()? {
        match f.num {
            1 => return Some(Value::Null),
            2 => {
                let bytes = f.data.as_slice().try_into().ok()?;
                return Some(serde_json::json!(f64::from_le_bytes(bytes)));
            }
            3 => return Some(Value::String(String::from_utf8(f.data).ok()?)),
            4 => return Some(serde_json::json!(f.n != 0)),
            5 => {
                let mut map = serde_json::Map::new();
                for e in checked_fields(&f.data)
                    .ok()?
                    .into_iter()
                    .filter(|e| e.num == 1)
                {
                    let kv = checked_fields(&e.data).ok()?;
                    let key = pb_str(&kv, 1);
                    let value = pb_any_at_depth(&field(&kv, 2)?.data, depth + 1)?;
                    map.insert(key, value);
                }
                return Some(Value::Object(map));
            }
            6 => {
                let items = checked_fields(&f.data)
                    .ok()?
                    .into_iter()
                    .filter(|e| e.num == 1)
                    .map(|e| pb_any_at_depth(&e.data, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                return Some(Value::Array(items));
            }
            _ => {}
        }
    }
    None
}

pub fn frame(msg: &[u8]) -> Vec<u8> {
    let mut head = vec![0u8; 5];
    let len = msg.len() as u32;
    head[1..5].copy_from_slice(&len.to_be_bytes());
    head.extend_from_slice(msg);
    head
}

pub struct Frame {
    pub end: bool,
    pub data: Vec<u8>,
}

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub struct FrameDecoder {
    buf: Vec<u8>,
    encoding: String,
}

impl FrameDecoder {
    pub fn new(encoding: &str) -> Self {
        Self {
            buf: Vec::new(),
            encoding: encoding.to_ascii_lowercase(),
        }
    }

    pub fn has_partial_frame(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, &'static str> {
        use std::io::Read;

        if self.buf.len().saturating_add(chunk.len()) > MAX_BATCH_BYTES {
            return Err("Cursor Connect 输入缓冲超出大小限制");
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut offset = 0;
        let mut decoded_bytes = 0;
        while let Some(header) = self.buf.get(offset..offset + 5) {
            let flags = header[0];
            if flags & !3 != 0 {
                return Err("Cursor Connect 帧标志无效");
            }
            let n = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
            if n > MAX_FRAME_BYTES {
                return Err("Cursor Connect 帧超出大小限制");
            }
            let end = offset + 5 + n;
            let Some(payload) = self.buf.get(offset + 5..end) else {
                break;
            };
            let data = if flags & 1 != 0 {
                if self.encoding != "gzip" {
                    return Err("Cursor Connect 帧使用了不支持的压缩方式");
                }
                let mut decoded = Vec::new();
                flate2::read::MultiGzDecoder::new(payload)
                    .take((MAX_FRAME_BYTES + 1) as u64)
                    .read_to_end(&mut decoded)
                    .map_err(|_| "Cursor Connect gzip 解压失败")?;
                if decoded.len() > MAX_FRAME_BYTES {
                    return Err("Cursor Connect 解压后帧超出大小限制");
                }
                decoded
            } else {
                payload.to_vec()
            };
            decoded_bytes += data.len() + 5;
            if decoded_bytes > MAX_BATCH_BYTES || out.len() >= 16_384 {
                return Err("Cursor Connect 帧批次超出大小限制");
            }
            out.push(Frame {
                end: flags & 2 != 0,
                data,
            });
            offset = end;
        }
        self.buf.drain(..offset);
        Ok(out)
    }
}

pub fn sha256_raw(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}
