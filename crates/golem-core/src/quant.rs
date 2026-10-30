//! A model file's quantization - `Q4_K_M`, `IQ4_XS`, `BF16`, `UD-Q4_K_XL` -
//! which is part of what the model *is*: the same weights in another quant
//! decode at another speed and answer differently, so a result names it and
//! is compared only with results of the same one.
//!
//! Two sources. The file name, where the publisher writes it and where it is
//! most specific (unsloth's `UD-Q4_K_XL` is `Q4_K_M` by the header's type);
//! and the GGUF header's `general.file_type`, for a file named without one.
//! A row recorded before quants were recorded carries only its file name,
//! which is the first source again.

/// The quant a file name carries: the last `-` or `.` separated part that
/// reads as one (`Q4_K_M`, `IQ4_XS`, `Q8_0`, `BF16`, `F16`, `MXFP4`, ...),
/// upper-cased, with unsloth's `UD-` kept in front. A split file's
/// `-00001-of-00002` is not part of it.
pub fn from_name(file: &str) -> Option<String> {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let stem = base.strip_suffix(".gguf").or_else(|| base.strip_suffix(".GGUF")).unwrap_or(base);
    let parts: Vec<&str> = stem.split(['-', '.']).collect();
    let at = parts.iter().rposition(|p| is_quant(p))?;
    let label = parts[at].to_ascii_uppercase();
    if at > 0 && parts[at - 1].eq_ignore_ascii_case("ud") {
        return Some(format!("UD-{label}"));
    }
    Some(label)
}

fn is_quant(part: &str) -> bool {
    let upper = part.to_ascii_uppercase();
    if !upper.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return false;
    }
    if matches!(upper.as_str(), "BF16" | "F16" | "F32" | "FP16" | "FP32") || upper.starts_with("MXFP4") {
        return true;
    }
    let rest = upper
        .strip_prefix("IQ")
        .or_else(|| upper.strip_prefix("TQ"))
        .or_else(|| upper.strip_prefix('Q'));
    // `Q4_K_M`, `Q8_0`, `IQ4_XS`, `TQ1_0`: a digit, then an underscore part.
    match rest {
        Some(r) => r.chars().next().is_some_and(|c| c.is_ascii_digit()) && r.contains('_'),
        None => false,
    }
}

/// The name of a GGUF `general.file_type` (llama.cpp's `llama_ftype`).
pub fn from_file_type(file_type: u32) -> Option<&'static str> {
    Some(match file_type {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4",
        _ => return None,
    })
}

/// A file's quant: its name's when the name carries one, else the header's
/// type, else empty.
pub fn label(file: &str, file_type: Option<u32>) -> String {
    from_name(file)
        .or_else(|| file_type.and_then(from_file_type).map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_says_the_quant() {
        assert_eq!(from_name("Ornith-1.5-35B-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(from_name("Qwen2.5-0.5B-Instruct-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(from_name("Qwen3.5-9B-UD-Q4_K_XL.gguf").as_deref(), Some("UD-Q4_K_XL"));
        assert_eq!(from_name("gemma-4-12b-it-qat-q4_0.gguf").as_deref(), Some("Q4_0"));
        assert_eq!(from_name("Model-IQ4_XS.gguf").as_deref(), Some("IQ4_XS"));
        assert_eq!(from_name("BF16/Qwen3.5-9B-BF16-00001-of-00002.gguf").as_deref(), Some("BF16"));
        assert_eq!(from_name("gpt-oss-20b-MXFP4.gguf").as_deref(), Some("MXFP4"));
        assert_eq!(from_name("Qwen3.6-35B-A3B.gguf"), None);
        assert_eq!(from_name("model.gguf"), None);
    }

    #[test]
    fn the_header_answers_for_a_name_without_one() {
        assert_eq!(from_file_type(15), Some("Q4_K_M"));
        assert_eq!(from_file_type(32), Some("BF16"));
        assert_eq!(from_file_type(4), None);
        assert_eq!(label("model.gguf", Some(30)), "IQ4_XS");
        assert_eq!(label("Thing-Q6_K.gguf", Some(15)), "Q6_K", "the name is more specific");
        assert_eq!(label("model.gguf", None), "");
    }
}
