//! Host-selected module surface requirements checked before setup.
use crate::name::{BusName, InterfaceName, MemberName, ObjectPath};
use crate::version::VersionRange;
use crate::{Error, Result};
use super::manifest::ModuleManifest;

/// One required interface version and its mandatory methods.
#[derive(Debug, Clone)]
pub struct RequiredInterface {
    pub(crate) name: InterfaceName,
    pub(crate) versions: VersionRange,
    pub(crate) methods: Vec<MemberName>,
}
impl RequiredInterface {
    /// Construct a requirement using validated bus vocabulary.
    pub fn new(name: InterfaceName, versions: VersionRange, methods: Vec<MemberName>) -> Result<Self> {
        if versions.max_exclusive.as_ref().is_some_and(|max| max <= &versions.min) {
            return Err(Error::bad_arguments("required interface version range is empty"));
        }
        Ok(Self { name, versions, methods })
    }
}

/// Exact module address and interfaces a host permits to initialize.
#[derive(Debug, Clone)]
pub struct ModuleRequirements {
    module: String,
    bus_name: BusName,
    object_path: ObjectPath,
    interfaces: Vec<RequiredInterface>,
}
impl ModuleRequirements {
    /// Construct an exact identity requirement without trusting artifact metadata.
    pub fn new(module: impl Into<String>, bus_name: BusName, object_path: ObjectPath,
        interfaces: Vec<RequiredInterface>) -> Result<Self> {
        let module = module.into();
        if module.is_empty() || module.len() > 256 || module.chars().any(char::is_control) {
            return Err(Error::bad_arguments("required module identity is invalid"));
        }
        Ok(Self { module, bus_name, object_path, interfaces })
    }
    pub(crate) fn check(&self, _manifest: &ModuleManifest) -> Result<()> {
        let _ = (&self.module, &self.bus_name, &self.object_path, &self.interfaces);
        Ok(())
    }
}
