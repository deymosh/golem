//! GGUF header reader: what a model file *is*, without loading it.
//!
//! The header holds the metadata (architecture, layer and expert counts,
//! trained context) and the tensor table; the weights after it are never
//! touched, so this is instant even for a 100 GB file. The recorder writes
//! these numbers into every row, so a change in what it reads is a change in
//! every row from then on.
//!
//! The metadata can be several megabytes (a 150k-token vocabulary is a
//! string array), so values are streamed past rather than collected: the
//! reader seeks over the parts it does not need and holds only the handful
//! of keys the report and the fit prediction use. The same reader takes the
//! first bytes of a file fetched by a range request ([`read_bytes`]), which
//! is how a model is sized before it is downloaded.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// GGUF metadata value types.
mod vtype {
    pub const UINT8: u32 = 0;
    pub const INT8: u32 = 1;
    pub const UINT16: u32 = 2;
    pub const INT16: u32 = 3;
    pub const UINT32: u32 = 4;
    pub const INT32: u32 = 5;
    pub const FLOAT32: u32 = 6;
    pub const BOOL: u32 = 7;
    pub const STRING: u32 = 8;
    pub const ARRAY: u32 = 9;
    pub const UINT64: u32 = 10;
    pub const INT64: u32 = 11;
    pub const FLOAT64: u32 = 12;
}

/// A metadata value. A short array of integers is kept - a per-layer head
/// count is one - and every other array is skipped as it is read (the
/// vocabulary is one, and it is megabytes).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Uint(u64),
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// One entry per layer, negatives read as 0.
    Ints(Vec<u64>),
}

/// The longest integer array kept: a layer count, with room to spare.
const INTS_MAX: u64 = 4096;

impl Value {
    /// The number this value holds: a string reads as 0 and a negative as 0,
    /// so an odd value never panics a reader that only wanted a count. A
    /// per-layer array reads as its largest entry.
    pub fn as_u64(&self) -> u64 {
        match self {
            Value::Uint(u) => *u,
            Value::Int(i) => (*i).max(0) as u64,
            Value::Float(f) => *f as u64,
            Value::Bool(b) => u64::from(*b),
            Value::Str(_) => 0,
            Value::Ints(items) => items.iter().copied().max().unwrap_or(0),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            _ => "",
        }
    }
}

/// What the recorder records about a model, and what the tuner needs to
/// decide how far a context may grow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFacts {
    pub arch: String,
    pub name: String,
    pub n_layer: u64,
    pub n_embd: u64,
    pub n_ctx_train: u64,
    pub n_expert: u64,
    pub n_expert_used: u64,
    pub moe: bool,
    pub params_total: u64,
    /// The parameters that do work per token: for a mixture of experts only
    /// the routed experts that fire, so a 35B-A3B is 35.5B of weights and
    /// 3.5B of arithmetic.
    pub params_active: u64,
    /// 1 when the file embeds an MTP/draft head (`blk.N.nextn.*`).
    pub mtp_tensors: u8,
    pub tensors: u64,
    /// `general.file_type`: the file's quant as llama.cpp numbers it
    /// (`crate::quant::from_file_type` names it), when the header says.
    pub file_type: Option<u32>,
    /// The KV cache's geometry: attention heads, the KV heads (the largest
    /// layer's, when the header gives one per layer) and each head's key and
    /// value width.
    pub n_head: u64,
    pub n_head_kv: u64,
    pub key_length: u64,
    pub value_length: u64,
    /// The layers that keep a KV cache that grows with the context, MTP
    /// layers left out: every layer, or in a hybrid model only every
    /// `full_attention_interval`-th - the rest keep a fixed recurrent state.
    pub n_layer_kv: u64,
    /// The MTP head's layers (`blk.N.nextn.*`), which keep a cache of their
    /// own when the engine drafts with them.
    pub nextn_layers: u64,
    /// Each tensor's name and size in bytes, from the offsets the tensor
    /// table gives (the last one from the file's size), in table order.
    pub tensor_bytes: Vec<(String, u64)>,
}

impl ModelFacts {
    /// The facts as `key=value` lines, one per fact - what the ignored
    /// local-model test prints for a person to read.
    pub fn to_info_text(&self) -> String {
        format!(
            "arch={}\nname={}\nn_layer={}\nn_embd={}\nn_ctx_train={}\nn_expert={}\nn_expert_used={}\nmoe={}\nparams_total={}\nparams_active={}\nmtp_tensors={}\ntensors={}\n",
            self.arch,
            self.name,
            self.n_layer,
            self.n_embd,
            self.n_ctx_train,
            self.n_expert,
            self.n_expert_used,
            if self.moe { "yes" } else { "no" },
            self.params_total,
            self.params_active,
            self.mtp_tensors,
            self.tensors,
        )
    }
}

#[derive(Debug)]
pub enum GgufError {
    Io(io::Error),
    /// The file does not start with the GGUF magic.
    NotGguf,
    /// The header ended in the middle of a value.
    Truncated,
}

impl fmt::Display for GgufError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GgufError::Io(e) => write!(f, "{e}"),
            GgufError::NotGguf => write!(f, "not a GGUF file"),
            GgufError::Truncated => write!(f, "header ends in the middle of a value"),
        }
    }
}

impl std::error::Error for GgufError {}

impl From<io::Error> for GgufError {
    fn from(e: io::Error) -> Self {
        // A read that hits the end is a truncated header, not an I/O fault.
        if e.kind() == io::ErrorKind::UnexpectedEof {
            GgufError::Truncated
        } else {
            GgufError::Io(e)
        }
    }
}

/// Where the header's bytes come from: a file, or the first bytes of one a
/// range request fetched. Both step over what is only in the way without
/// reading it into memory.
trait Source: Read {
    fn skip_bytes(&mut self, n: u64) -> io::Result<()>;
}

impl Source for BufReader<File> {
    fn skip_bytes(&mut self, n: u64) -> io::Result<()> {
        // At most the file's length, which fits an i64 (`Cursor::skip`).
        self.seek_relative(n as i64)
    }
}

impl Source for io::Cursor<&[u8]> {
    fn skip_bytes(&mut self, n: u64) -> io::Result<()> {
        // Past the end is allowed here: the next read then fails, and that
        // is the truncated header it is.
        self.set_position(self.position().saturating_add(n));
        Ok(())
    }
}

/// A forward-only reader over the header, with seeking for the parts that
/// are only in the way.
struct Cursor<R: Source> {
    reader: R,
    /// The file's length: no length field in the header can be larger, so
    /// one that is says the header is broken before anything is allocated
    /// or skipped for it.
    size: u64,
    /// How far into the file the reader is: where the header ends, once it
    /// is read, and so where the tensor data starts.
    pos: u64,
}

impl<R: Source> Cursor<R> {
    fn take(&mut self, n: usize) -> Result<Vec<u8>, GgufError> {
        if n as u64 > self.size {
            return Err(GgufError::Truncated);
        }
        let mut buf = vec![0u8; n];
        self.reader.read_exact(&mut buf)?;
        self.pos += n as u64;
        Ok(buf)
    }

    fn u8(&mut self) -> Result<u8, GgufError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, GgufError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, GgufError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, GgufError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    fn i8(&mut self) -> Result<i8, GgufError> {
        Ok(self.u8()? as i8)
    }

    fn i16(&mut self) -> Result<i16, GgufError> {
        Ok(self.u16()? as i16)
    }

    fn i32(&mut self) -> Result<i32, GgufError> {
        Ok(self.u32()? as i32)
    }

    fn i64(&mut self) -> Result<i64, GgufError> {
        Ok(self.u64()? as i64)
    }

    fn f32(&mut self) -> Result<f32, GgufError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn f64(&mut self) -> Result<f64, GgufError> {
        Ok(f64::from_bits(self.u64()?))
    }

    /// A GGUF string: a u64 length, then that many bytes of UTF-8.
    fn string(&mut self) -> Result<String, GgufError> {
        let len = self.u64()?;
        let len = usize::try_from(len).map_err(|_| GgufError::Truncated)?;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Step over `n` bytes without reading them into memory: forward only,
    /// and never past the end of the file.
    fn skip(&mut self, n: u64) -> Result<(), GgufError> {
        if n > self.size {
            return Err(GgufError::Truncated);
        }
        if n > 0 {
            self.reader.skip_bytes(n)?;
            self.pos += n;
        }
        Ok(())
    }

    /// The fixed width of a value type, or None for strings and arrays.
    fn fixed_width(t: u32) -> Option<u64> {
        match t {
            vtype::UINT8 | vtype::INT8 | vtype::BOOL => Some(1),
            vtype::UINT16 | vtype::INT16 => Some(2),
            vtype::UINT32 | vtype::INT32 | vtype::FLOAT32 => Some(4),
            vtype::UINT64 | vtype::INT64 | vtype::FLOAT64 => Some(8),
            _ => None,
        }
    }

    /// Read a value, or step over it when it is one of the kinds we do not
    /// keep (floats we do keep - they are small - and arrays only when they
    /// are short and of integers).
    fn value(&mut self, t: u32) -> Result<Option<Value>, GgufError> {
        Ok(match t {
            vtype::UINT8 => Some(Value::Uint(self.u8()? as u64)),
            vtype::INT8 => Some(Value::Int(self.i8()? as i64)),
            vtype::UINT16 => Some(Value::Uint(self.u16()? as u64)),
            vtype::INT16 => Some(Value::Int(self.i16()? as i64)),
            vtype::UINT32 => Some(Value::Uint(self.u32()? as u64)),
            vtype::INT32 => Some(Value::Int(self.i32()? as i64)),
            vtype::FLOAT32 => Some(Value::Float(self.f32()? as f64)),
            vtype::BOOL => Some(Value::Bool(self.u8()? != 0)),
            vtype::STRING => Some(Value::Str(self.string()?)),
            vtype::UINT64 => Some(Value::Uint(self.u64()?)),
            vtype::INT64 => Some(Value::Int(self.i64()?)),
            vtype::FLOAT64 => Some(Value::Float(self.f64()?)),
            vtype::ARRAY => self.array()?,
            // An unknown type cannot be sized, so the header cannot be
            // trusted past it: better to say so than to report nonsense.
            _ => return Err(GgufError::Truncated),
        })
    }

    /// An array is an element type, a count, then the elements. A short one
    /// of integers is kept ([`Value::Ints`]). Other fixed-width elements are
    /// stepped over in one go; strings are walked one at a time (their length
    /// varies), which is what makes the tokenizer vocabulary cheap: it is
    /// read as lengths and skipped, never held.
    fn array(&mut self) -> Result<Option<Value>, GgufError> {
        let et = self.u32()?;
        let count = self.u64()?;
        if et == vtype::STRING {
            for _ in 0..count {
                let len = self.u64()?;
                self.skip(len)?;
            }
            return Ok(None);
        }
        let integer = !matches!(et, vtype::FLOAT32 | vtype::FLOAT64 | vtype::BOOL);
        if integer && count <= INTS_MAX {
            let mut items = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let item = self.value(et)?.map_or(0, |v| v.as_u64());
                items.push(item);
            }
            return Ok(Some(Value::Ints(items)));
        }
        match Self::fixed_width(et) {
            Some(w) => self.skip(w.saturating_mul(count)).map(|()| None),
            None => Err(GgufError::Truncated),
        }
    }
}

/// Read a model's facts from its GGUF header.
pub fn read(path: &Path) -> Result<ModelFacts, GgufError> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    facts(Cursor { reader: BufReader::with_capacity(1 << 16, file), size, pos: 0 })
}

/// Read a model's facts from the first bytes of its file - what a range
/// request fetched - given the whole file's size (the `Content-Range`
/// total), which sizes the last tensor. `Truncated` means the header runs
/// past `bytes`: fetch more and read again.
pub fn read_bytes(bytes: &[u8], file_size: u64) -> Result<ModelFacts, GgufError> {
    facts(Cursor { reader: io::Cursor::new(bytes), size: file_size, pos: 0 })
}

/// Where tensor data starts when the header does not say: GGUF's default
/// `general.alignment`.
const ALIGNMENT: u64 = 32;

fn facts<R: Source>(mut c: Cursor<R>) -> Result<ModelFacts, GgufError> {
    let magic = c.take(4)?;
    if magic != *b"GGUF" {
        return Err(GgufError::NotGguf);
    }
    let _version = c.u32()?;
    let n_tensors = c.u64()?;
    let n_kv = c.u64()?;

    let mut kv: HashMap<String, Value> = HashMap::new();
    for _ in 0..n_kv {
        let key = c.string()?;
        let t = c.u32()?;
        if let Some(value) = c.value(t)? {
            kv.insert(key, value);
        }
    }

    let mut params_total: u64 = 0;
    let mut routed: u64 = 0;
    let mut mtp: u64 = 0;
    let mut nextn_blocks: Vec<u64> = Vec::new();
    let mut offsets: Vec<(String, u64)> = Vec::with_capacity(n_tensors.min(1 << 16) as usize);
    for _ in 0..n_tensors {
        let name = c.string()?;
        let n_dims = c.u32()?;
        let mut count: u64 = 1;
        for _ in 0..n_dims {
            count = count.saturating_mul(c.u64()?);
        }
        let _quant = c.u32()?;
        let offset = c.u64()?;
        params_total = params_total.saturating_add(count);
        // llama.cpp names routed experts blk.N.ffn_{gate,up,down}_exps; a
        // shared expert is "..._shexp" and runs on every token.
        if name.contains("_exps.") {
            routed = routed.saturating_add(count);
        }
        if name.contains(".nextn.") {
            mtp = mtp.saturating_add(count);
            if let Some(block) = block_of(&name) {
                if !nextn_blocks.contains(&block) {
                    nextn_blocks.push(block);
                }
            }
        }
        offsets.push((name, offset));
    }
    let alignment = kv.get("general.alignment").map(Value::as_u64).filter(|&a| a > 0).unwrap_or(ALIGNMENT);
    let data_start = c.pos.div_ceil(alignment) * alignment;
    let tensor_bytes = sizes(offsets, c.size.saturating_sub(data_start));

    let arch = kv.get("general.architecture").map(Value::as_str).unwrap_or("").to_string();
    // Spelled out rather than behind a closure: the closure would hold a
    // borrow of `arch` for the rest of the function, and the struct below
    // moves it.
    let n_layer = kv.get(&format!("{arch}.block_count")).map(Value::as_u64).unwrap_or(0);
    let n_embd = kv.get(&format!("{arch}.embedding_length")).map(Value::as_u64).unwrap_or(0);
    let n_ctx_train = kv.get(&format!("{arch}.context_length")).map(Value::as_u64).unwrap_or(0);
    let n_expert = kv.get(&format!("{arch}.expert_count")).map(Value::as_u64).unwrap_or(0);
    let n_expert_used =
        kv.get(&format!("{arch}.expert_used_count")).map(Value::as_u64).unwrap_or(0);

    let moe = n_expert > 1;
    let params_active = if moe && n_expert_used > 0 {
        // The routed experts are the only part that scales with how many
        // fire per token.
        // In u128: the counts are the header's word, and a broken one must
        // not overflow. `routed` is part of `params_total`, so the result
        // fits back.
        let active = routed as u128 * n_expert_used.min(n_expert) as u128 / n_expert as u128;
        params_total.saturating_sub(routed).saturating_add(active as u64)
    } else {
        params_total
    };

    let n_head = kv.get(&format!("{arch}.attention.head_count")).map(Value::as_u64).unwrap_or(0);
    let head_kv = kv.get(&format!("{arch}.attention.head_count_kv"));
    let n_head_kv = head_kv.map(Value::as_u64).unwrap_or(n_head);
    // Without the key the widths are the embedding split over the heads,
    // which is what llama.cpp assumes too.
    let head_width = if n_head > 0 { n_embd / n_head } else { 0 };
    let key_length = kv.get(&format!("{arch}.attention.key_length")).map(Value::as_u64).unwrap_or(head_width);
    let value_length = kv.get(&format!("{arch}.attention.value_length")).map(Value::as_u64).unwrap_or(head_width);
    let nextn_layers = nextn_blocks.len() as u64;
    let interval = kv.get(&format!("{arch}.full_attention_interval")).map(Value::as_u64).unwrap_or(0);
    let n_layer_kv = layers_with_cache(n_layer.saturating_sub(nextn_layers), interval, head_kv);

    Ok(ModelFacts {
        arch,
        name: kv.get("general.name").map(Value::as_str).unwrap_or("").to_string(),
        n_layer,
        n_embd,
        n_ctx_train,
        n_expert,
        n_expert_used,
        moe,
        params_total,
        params_active,
        mtp_tensors: u8::from(mtp > 0),
        tensors: n_tensors,
        file_type: kv.get("general.file_type").map(|v| v.as_u64() as u32),
        n_head,
        n_head_kv,
        key_length,
        value_length,
        n_layer_kv,
        nextn_layers,
        tensor_bytes,
    })
}

/// `blk.<N>.…` -> N.
fn block_of(name: &str) -> Option<u64> {
    name.strip_prefix("blk.")?.split('.').next()?.parse().ok()
}

/// How many of the `layers` keep a KV cache that grows with the context.
/// A per-layer KV head count says it layer by layer (a 0 keeps none); a
/// hybrid model's `full_attention_interval` keeps one in every n-th layer
/// (llama.cpp's `(il + 1) % n == 0`), the others recurrent; otherwise all.
fn layers_with_cache(layers: u64, interval: u64, head_kv: Option<&Value>) -> u64 {
    if let Some(Value::Ints(per_layer)) = head_kv {
        return per_layer.iter().take(layers as usize).filter(|&&n| n > 0).count() as u64;
    }
    if interval > 0 {
        return layers / interval;
    }
    layers
}

/// Each tensor's size, from the offsets in table order: the distance to the
/// next tensor's data, and for the last one to the end of `data_len` (the
/// file's bytes after the header). Padding between tensors counts towards
/// the one before it, as it does in memory once loaded.
fn sizes(offsets: Vec<(String, u64)>, data_len: u64) -> Vec<(String, u64)> {
    let mut by_offset: Vec<u64> = offsets.iter().map(|(_, o)| *o).collect();
    by_offset.sort_unstable();
    by_offset.dedup();
    offsets
        .into_iter()
        .map(|(name, offset)| {
            let next = match by_offset.binary_search(&offset) {
                Ok(i) => by_offset.get(i + 1).copied().unwrap_or(data_len),
                Err(_) => data_len,
            };
            (name, next.saturating_sub(offset))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- a tiny GGUF writer, so the tests need no model file ------------

    fn s(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(&(text.len() as u64).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }

    fn kv_str(out: &mut Vec<u8>, key: &str, value: &str) {
        s(out, key);
        out.extend_from_slice(&vtype::STRING.to_le_bytes());
        s(out, value);
    }

    fn kv_u32(out: &mut Vec<u8>, key: &str, value: u32) {
        s(out, key);
        out.extend_from_slice(&vtype::UINT32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }

    /// An array of strings, the shape a tokenizer vocabulary has.
    fn kv_str_array(out: &mut Vec<u8>, key: &str, items: &[&str]) {
        s(out, key);
        out.extend_from_slice(&vtype::ARRAY.to_le_bytes());
        out.extend_from_slice(&vtype::STRING.to_le_bytes());
        out.extend_from_slice(&(items.len() as u64).to_le_bytes());
        for item in items {
            s(out, item);
        }
    }

    /// A tensor with `elements` weights, all in one dimension.
    fn tensor(out: &mut Vec<u8>, name: &str, elements: u64) {
        s(out, name);
        out.extend_from_slice(&1u32.to_le_bytes()); // one dimension
        out.extend_from_slice(&elements.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // quant type
        out.extend_from_slice(&0u64.to_le_bytes()); // offset
    }

    fn write_gguf(
        n_kv: u64,
        kvs: impl FnOnce(&mut Vec<u8>),
        n_tensors: u64,
        tensors: impl FnOnce(&mut Vec<u8>),
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3u32.to_le_bytes()); // version
        out.extend_from_slice(&n_tensors.to_le_bytes());
        out.extend_from_slice(&n_kv.to_le_bytes());
        let mut body = Vec::new();
        kvs(&mut body);
        out.extend_from_slice(&body);
        let mut t = Vec::new();
        tensors(&mut t);
        out.extend_from_slice(&t);
        out
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("golem-rs-gguf-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let mut f = File::create(&path).expect("create");
        f.write_all(bytes).expect("write");
        path
    }

    fn dense_fixture() -> Vec<u8> {
        write_gguf(
            6,
            |b| {
                kv_str(b, "general.architecture", "qwen3");
                kv_str(b, "general.name", "Qwen Test 0.5B");
                kv_u32(b, "qwen3.block_count", 24);
                kv_u32(b, "qwen3.embedding_length", 896);
                kv_u32(b, "qwen3.context_length", 32768);
                // A vocabulary: the reader must step over it, not hold it.
                kv_str_array(b, "tokenizer.ggml.tokens", &["a", "bb", "ccc"]);
            },
            3,
            |b| {
                tensor(b, "token_embd.weight", 1_000);
                tensor(b, "blk.0.attn_q.weight", 2_000);
                tensor(b, "output.weight", 1_000);
            },
        )
    }

    fn moe_fixture() -> Vec<u8> {
        write_gguf(
            6,
            |b| {
                kv_str(b, "general.architecture", "qwen3_5_moe");
                kv_str(b, "general.name", "MoE Test");
                kv_u32(b, "qwen3_5_moe.block_count", 40);
                kv_u32(b, "qwen3_5_moe.context_length", 262144);
                kv_u32(b, "qwen3_5_moe.expert_count", 256);
                kv_u32(b, "qwen3_5_moe.expert_used_count", 8);
            },
            4,
            |b| {
                // 1000 dense + 256 experts x 100 = 25600 routed.
                tensor(b, "token_embd.weight", 1_000);
                tensor(b, "blk.0.ffn_gate_exps.weight", 25_600);
                tensor(b, "blk.0.ffn_gate_shexp.weight", 500); // shared: every token
                tensor(b, "blk.40.nextn.eh_proj.weight", 700); // MTP head
            },
        )
    }

    #[test]
    fn reads_a_dense_model() {
        let path = write_temp("dense.gguf", &dense_fixture());
        let facts = read(&path).expect("should read");
        assert_eq!(facts.arch, "qwen3");
        assert_eq!(facts.name, "Qwen Test 0.5B");
        assert_eq!(facts.n_layer, 24);
        assert_eq!(facts.n_embd, 896);
        assert_eq!(facts.n_ctx_train, 32768);
        assert!(!facts.moe);
        assert_eq!(facts.params_total, 4_000);
        assert_eq!(facts.params_active, 4_000, "a dense model does all its work per token");
        assert_eq!(facts.mtp_tensors, 0);
        assert_eq!(facts.tensors, 3);
    }

    #[test]
    fn counts_only_the_experts_that_fire_as_active() {
        let path = write_temp("moe.gguf", &moe_fixture());
        let facts = read(&path).expect("should read");
        assert!(facts.moe);
        assert_eq!(facts.n_expert, 256);
        assert_eq!(facts.n_expert_used, 8);
        // total = 1000 + 25600 + 500 + 700 = 27800
        assert_eq!(facts.params_total, 27_800);
        // active = total - routed + routed * 8/256
        //        = 27800 - 25600 + 800 = 3000
        assert_eq!(facts.params_active, 3_000);
        assert_eq!(facts.mtp_tensors, 1, "the .nextn. tensor marks the MTP head");
    }

    #[test]
    fn a_shared_expert_is_not_treated_as_routed() {
        // The shared expert runs on every token, so it must stay in the
        // active count: only `_exps.` names are the routed ones.
        let path = write_temp("shared.gguf", &moe_fixture());
        let facts = read(&path).expect("should read");
        let routed_only = facts.params_total - facts.params_active;
        // 25600 routed - 800 that fire = 24800 saved; the 500 shared expert
        // is not part of that saving.
        assert_eq!(routed_only, 24_800);
    }

    /// A tensor whose data starts `offset` bytes after the header.
    fn tensor_at(out: &mut Vec<u8>, name: &str, elements: u64, offset: u64) {
        s(out, name);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&elements.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
    }

    /// The 35B's shape in small: 41 blocks, the last the MTP head, a KV
    /// cache in every fourth of the other 40 - the keys as its header has
    /// them - and three tensors whose data is 64, 128 and 64 bytes, after
    /// the header padded to 32.
    fn hybrid_fixture() -> Vec<u8> {
        let mut bytes = write_gguf(
            7,
            |b| {
                kv_str(b, "general.architecture", "qwen35moe");
                kv_u32(b, "qwen35moe.block_count", 41);
                kv_u32(b, "qwen35moe.attention.head_count", 16);
                kv_u32(b, "qwen35moe.attention.head_count_kv", 2);
                kv_u32(b, "qwen35moe.attention.key_length", 256);
                kv_u32(b, "qwen35moe.attention.value_length", 256);
                kv_u32(b, "qwen35moe.full_attention_interval", 4);
            },
            3,
            |b| {
                tensor_at(b, "blk.0.attn_q.weight", 16, 0);
                tensor_at(b, "blk.0.ffn_gate_exps.weight", 32, 64);
                tensor_at(b, "blk.40.nextn.eh_proj.weight", 16, 192);
            },
        );
        bytes.resize(bytes.len().div_ceil(32) * 32 + 256, 0);
        bytes
    }

    #[test]
    fn a_hybrid_models_cache_geometry_and_tensor_sizes_are_read() {
        let bytes = hybrid_fixture();
        let facts = read_bytes(&bytes, bytes.len() as u64).expect("reads");
        assert_eq!((facts.n_head, facts.n_head_kv, facts.key_length, facts.value_length), (16, 2, 256, 256));
        assert_eq!(facts.nextn_layers, 1, "block 40 is the MTP head");
        assert_eq!(facts.n_layer_kv, 10, "every fourth of the 40 others");
        assert_eq!(
            facts.tensor_bytes,
            vec![
                ("blk.0.attn_q.weight".to_string(), 64),
                ("blk.0.ffn_gate_exps.weight".to_string(), 128),
                ("blk.40.nextn.eh_proj.weight".to_string(), 64),
            ]
        );
        // The file reader agrees with the byte reader.
        let path = write_temp("hybrid.gguf", &bytes);
        assert_eq!(read(&path).expect("reads"), facts);
    }

    #[test]
    fn a_per_layer_kv_head_count_says_which_layers_keep_a_cache() {
        let bytes = write_gguf(
            3,
            |b| {
                kv_str(b, "general.architecture", "hybrid");
                kv_u32(b, "hybrid.block_count", 4);
                s(b, "hybrid.attention.head_count_kv");
                b.extend_from_slice(&vtype::ARRAY.to_le_bytes());
                b.extend_from_slice(&vtype::UINT32.to_le_bytes());
                b.extend_from_slice(&4u64.to_le_bytes());
                for n in [0u32, 8, 0, 4] {
                    b.extend_from_slice(&n.to_le_bytes());
                }
            },
            1,
            |b| tensor(b, "token_embd.weight", 10),
        );
        let facts = read_bytes(&bytes, bytes.len() as u64).expect("reads");
        assert_eq!(facts.n_layer_kv, 2);
        assert_eq!(facts.n_head_kv, 8, "the largest layer's");
    }

    #[test]
    fn a_range_short_of_the_header_is_truncated() {
        let bytes = hybrid_fixture();
        let err = read_bytes(&bytes[..100], bytes.len() as u64).expect_err("cut inside the header");
        assert!(matches!(err, GgufError::Truncated), "{err}");
    }

    #[test]
    fn rejects_a_file_that_is_not_gguf() {
        let path = write_temp("nope.bin", b"NOTGGUF and then some");
        assert!(matches!(read(&path), Err(GgufError::NotGguf)));
    }

    #[test]
    fn reports_a_truncated_header_instead_of_guessing() {
        let mut bytes = dense_fixture();
        bytes.truncate(20);
        let path = write_temp("short.gguf", &bytes);
        assert!(matches!(read(&path), Err(GgufError::Truncated)));
    }

    #[test]
    fn a_missing_key_reads_as_zero() {
        // An absent `<arch>.context_length` is 0, not an error: an odd file
        // still yields the facts it does have.
        let bytes = write_gguf(
            1,
            |b| kv_str(b, "general.architecture", "mystery"),
            1,
            |b| tensor(b, "token_embd.weight", 10),
        );
        let path = write_temp("odd.gguf", &bytes);
        let facts = read(&path).expect("should read");
        assert_eq!(facts.arch, "mystery");
        assert_eq!(facts.n_ctx_train, 0);
        assert_eq!(facts.n_layer, 0);
        assert_eq!(facts.params_total, 10);
    }

    #[test]
    fn info_text_is_one_key_value_line_per_fact() {
        let path = write_temp("text.gguf", &dense_fixture());
        let text = read(&path).expect("should read").to_info_text();
        assert!(text.starts_with("arch=qwen3\nname=Qwen Test 0.5B\n"));
        assert!(text.contains("\nmoe=no\n"));
        assert!(text.ends_with("tensors=3\n"));
    }

    /// Every GGUF this machine has, read end to end. Ignored by default
    /// because it needs the model files (git-ignored, not on CI):
    ///
    ///     ./scripts/cargo.sh test -- --ignored --nocapture
    ///
    /// The check: every real model's header parses, and the fields the tuner
    /// sizes a context from are there.
    #[test]
    #[ignore = "needs data/models/*.gguf"]
    fn every_local_model_reads() {
        // crates/golem-core -> the repository root is three levels up.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let models = root.join("data/models");
        let entries = std::fs::read_dir(&models).unwrap_or_else(|e| {
            panic!(
                "cannot read {}: {e} - this test needs the downloaded models",
                models.display()
            )
        });
        let mut checked = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("gguf") {
                continue;
            }
            let info = read(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            // A header that parses but says nothing is the failure this
            // catches: a truncated download, or a key this reader reads from
            // the wrong place.
            assert!(
                info.n_layer > 0,
                "{} has no layer count - the tuner seeds a split from it",
                path.display()
            );
            assert!(
                info.n_ctx_train > 0,
                "{} has no trained context - the context ceiling comes from it",
                path.display()
            );
            eprintln!("{}: {}", path.file_name().unwrap_or_default().to_string_lossy(), info.to_info_text().replace('\n', " | "));
            checked += 1;
        }
        assert!(checked > 0, "no .gguf files found in {}", models.display());
        eprintln!("read {checked} model(s)");
    }
}
