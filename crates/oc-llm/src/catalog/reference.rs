//! `provider/id[#variant]` reference parsing.

use crate::error::LlmError;

/// Parse the reference syntax once. `provider/id[#variant]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
    pub variant: Option<String>,
}

pub fn parse_reference(reference: &str) -> Result<ModelRef, LlmError> {
    let invalid = || LlmError::InvalidReference {
        reference: reference.to_string(),
    };
    let provider_end = reference.find('/').ok_or_else(invalid)?;
    if provider_end == 0 {
        return Err(invalid());
    }
    let provider = &reference[..provider_end];
    if provider.contains('#') {
        return Err(invalid());
    }
    let rest = &reference[provider_end + 1..];
    let (id, variant) = match rest.find('#') {
        Some(hash) => (&rest[..hash], Some(&rest[hash + 1..])),
        None => (rest, None),
    };
    if id.is_empty() {
        return Err(invalid());
    }
    if let Some(variant) = variant
        && (variant.is_empty() || variant.contains('#'))
    {
        return Err(invalid());
    }
    Ok(ModelRef {
        provider: provider.to_string(),
        id: id.to_string(),
        variant: variant.map(str::to_string),
    })
}
