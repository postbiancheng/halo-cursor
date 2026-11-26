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
    let mut out = Vec::new();
    let mut i = 0usize;
    let uv = |buf: &[u8], i: &mut usize| -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        while *i < buf.len() {
            let c = buf[*i];
            *i += 1;
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
            break;
        };
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
                    break;
                };
                field.n = v;
            }
            1 => {
                if i + 8 > input.len() {
                    break;
                }
                field.data = input[i..i + 8].to_vec();
                field.n = u64::from_le_bytes(input[i..i + 8].try_into().unwrap_or([0; 8]));
                i += 8;
            }
            2 => {
                let Some(len) = uv(input, &mut i) else {
                    break;
                };
                let len = len as usize;
                if i + len > input.len() {
                    break;
                }
                field.data = input[i..i + len].to_vec();
                i += len;
            }
            5 => {
                if i + 4 > input.len() {
                    break;
                }
                field.n = u32::from_le_bytes(input[i..i + 4].try_into().unwrap_or([0; 4])) as u64;
                i += 4;
            }
            _ => break,
        }
        out.push(field);
    }
    out
}

pub fn field<'a>(fs: &'a [Field], num: u32) -> Option<&'a Field> {
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

pub fn pb_any(data: &[u8]) -> serde_json::Value {
    for f in fields(data) {
        match f.num {
            1 => return serde_json::Value::Null,
            2 => {
                if f.data.len() == 8 {
                    let bytes: [u8; 8] = f.data.as_slice().try_into().unwrap_or([0; 8]);
                    return serde_json::json!(f64::from_le_bytes(bytes));
                }
                return serde_json::json!(0.0);
            }
            3 => return serde_json::Value::String(String::from_utf8_lossy(&f.data).into_owned()),
            4 => return serde_json::json!(f.n != 0),
            5 => {
                let mut map = serde_json::Map::new();
                for e in fields(&f.data) {
                    if e.num != 1 {
                        continue;
                    }
                    let kv = fields(&e.data);
                    let key = pb_str(&kv, 1);
                    let value = field(&kv, 2)
                        .map(|v| pb_any(&v.data))
                        .unwrap_or(serde_json::Value::Null);
                    map.insert(key, value);
                }
                return serde_json::Value::Object(map);
            }
            6 => {
                let items: Vec<_> = fields(&f.data)
                    .into_iter()
                    .filter(|e| e.num == 1)
                    .map(|e| pb_any(&e.data))
                    .collect();
                return serde_json::Value::Array(items);
            }
            _ => {}
        }
    }
    serde_json::Value::Null
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

pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while self.buf.len() >= 5 {
            let n = u32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize;
            if n > 64 << 20 {
                return Err("malformed Connect stream".into());
            }
            if self.buf.len() < 5 + n {
                break;
            }
            let flags = self.buf[0];
            let mut data = self.buf[5..5 + n].to_vec();
            self.buf.drain(..5 + n);
            if flags & 1 != 0 {
                // gzip: WASM host rarely compresses; keep raw on failure
                // Agent2API http sets accept-encoding: identity for RunSSE.
            }
            if flags & 1 != 0 {
                // Ignore gzip bit if still compressed; Magpie gunzips. We request identity.
                let _ = &mut data;
            }
            out.push(Frame {
                end: flags & 2 != 0,
                data,
            });
        }
        Ok(out)
    }
}

pub fn sha256_raw(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}
