//! Bound the owned command DTO before copying Tauri's borrowed JSON payload.
//!
//! Tauri's transport has already parsed JSON into a Value. These guards bound
//! our second allocation and reject oversized containers without iterating them;
//! transport-level request limits remain the webview host's responsibility.

use std::io::Write;

/// An IPC argument with byte and top-level container caps checked before DTO
/// deserialization. Rust callers may construct it from an already-owned value.
pub struct BoundedArgument<T, const BYTES: usize, const ITEMS: usize>(pub T);

impl<T, const BYTES: usize, const ITEMS: usize> From<T> for BoundedArgument<T, BYTES, ITEMS> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

struct Budget(usize);
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IPC argument cap exceeded",
            )
        })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'de, R, T, const BYTES: usize, const ITEMS: usize> tauri::ipc::CommandArg<'de, R>
    for BoundedArgument<T, BYTES, ITEMS>
where
    R: tauri::Runtime,
    T: serde::de::DeserializeOwned,
{
    fn from_command(
        command: tauri::ipc::CommandItem<'de, R>,
    ) -> Result<Self, tauri::ipc::InvokeError> {
        let tauri::ipc::InvokeBody::Json(payload) = command.message.payload() else {
            return Err("MCP commands require JSON arguments".into());
        };
        let value = payload
            .get(command.key)
            .ok_or_else(|| tauri::ipc::InvokeError::from("missing MCP argument"))?;
        let count = match value {
            serde_json::Value::Array(values) => values.len(),
            serde_json::Value::Object(values) => values.len(),
            _ => 0,
        };
        if count > ITEMS {
            return Err("IPC argument cap exceeded".into());
        }
        // Count serialized bytes into a non-allocating, fail-fast sink. This
        // includes escaped strings and bounds nested values as well as fields
        // which serde would otherwise ignore while consuming an entry.
        serde_json::to_writer(Budget(BYTES), value)
            .map_err(|_| tauri::ipc::InvokeError::from("IPC argument cap exceeded"))?;
        T::deserialize(value)
            .map(Self)
            .map_err(|e| e.to_string().into())
    }
}
