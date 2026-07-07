use std::fmt;

/// A capability a node may exercise at runtime.
///
/// These are declarations carried by the IR; from S3 on the backend confines
/// the compiled artifact to exactly the declared set at the wasm/wasi boundary.
/// An undeclared capability is a validation failure at compile time and a trap
/// at run time — never a silent grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilityEffect {
    /// open sockets / make requests
    Net,
    /// touch the filesystem
    Fs,
    /// spawn processes / execute code outside the graph
    Exec,
    /// read secret material (api keys, credentials)
    Secrets,
    /// move money
    Payments,
}

/// An effect against the task's memory store (not the filesystem — that is
/// [`CapabilityEffect::Fs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemoryEffect {
    /// consult the store
    Read,
    /// overwrite a key
    Write,
    /// append to a log-shaped key
    Append,
}

/// How much of a node's behavior is fixed by its inputs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Uncertainty {
    /// same inputs → same outputs, always; candidate for symbolic extraction
    #[default]
    Deterministic,
    /// output distribution is narrow and checkable (retries, sampling with
    /// low temperature, flaky externals)
    Probabilistic,
    /// open-ended production; outputs are judged by contract, not equality
    Generative,
}

impl fmt::Display for CapabilityEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Net => "net",
            Self::Fs => "fs",
            Self::Exec => "exec",
            Self::Secrets => "secrets",
            Self::Payments => "payments",
        })
    }
}

impl fmt::Display for MemoryEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Append => "append",
        })
    }
}

impl fmt::Display for Uncertainty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Deterministic => "deterministic",
            Self::Probabilistic => "probabilistic",
            Self::Generative => "generative",
        })
    }
}
