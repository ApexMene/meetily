//! Hugging Face token storage in the OS credential store (Windows Credential Manager,
//! macOS Keychain, Linux keyutils). The token is never written to SQLite or to logs.

const SERVICE: &str = "com.meetily.ai.huggingface";
const ACCOUNT: &str = "hf-token";

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| format!("Credential store unavailable: {}", e))
}

pub(crate) fn get_token() -> Result<Option<String>, String> {
    match entry()?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("Cannot read the Hugging Face token: {}", error)),
    }
}

pub(crate) fn set_token(token: &str) -> Result<(), String> {
    let token = token.trim();
    // HF user access tokens look like `hf_` + alphanumerics; reject anything that could break
    // an HTTP header or is obviously not a token.
    if token.len() < 8
        || token.len() > 256
        || !token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("This does not look like a Hugging Face access token".into());
    }
    entry()?
        .set_password(token)
        .map_err(|e| format!("Cannot save the Hugging Face token: {}", e))
}

pub(crate) fn clear_token() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("Cannot remove the Hugging Face token: {}", error)),
    }
}
