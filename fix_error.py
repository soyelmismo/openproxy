import re
with open("crates/openproxy-types/src/error.rs", "r") as f:
    text = f.read()

text = text.replace("""pub type Result<T> = std::result::Result<T, CoreError>;""", """pub type Result<T> = std::result::Result<T, CoreError>;

impl From<tokio::task::JoinError> for CoreError {
    fn from(err: tokio::task::JoinError) -> Self {
        CoreError::Internal(err.to_string())
    }
}
""")

with open("crates/openproxy-types/src/error.rs", "w") as f:
    f.write(text)
