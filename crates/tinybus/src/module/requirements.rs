//! Host-selected module surface requirements checked before setup.
use super::manifest::ModuleManifest;
use crate::name::{BusName, InterfaceName, MemberName, ObjectPath};
use crate::version::VersionRange;
use crate::{Error, Result};

/// One required interface version and its mandatory methods.
#[derive(Debug, Clone)]
pub struct RequiredInterface {
    pub(crate) name: InterfaceName,
    pub(crate) versions: VersionRange,
    pub(crate) methods: Vec<MemberName>,
}
impl RequiredInterface {
    /// Construct a requirement using validated bus vocabulary.
    pub fn new(
        name: InterfaceName,
        versions: VersionRange,
        methods: Vec<MemberName>,
    ) -> Result<Self> {
        if versions
            .max_exclusive
            .as_ref()
            .is_some_and(|max| max <= &versions.min)
        {
            return Err(Error::Protocol(
                "required interface version range is empty".into(),
            ));
        }
        Ok(Self {
            name,
            versions,
            methods,
        })
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
    pub fn new(
        module: impl Into<String>,
        bus_name: BusName,
        object_path: ObjectPath,
        interfaces: Vec<RequiredInterface>,
    ) -> Result<Self> {
        let module = module.into();
        if module.is_empty() || module.len() > 256 || module.chars().any(char::is_control) {
            return Err(Error::Protocol(
                "required module identity is invalid".into(),
            ));
        }
        Ok(Self {
            module,
            bus_name,
            object_path,
            interfaces,
        })
    }
    pub(crate) fn check(&self, manifest: &ModuleManifest) -> Result<()> {
        if manifest.module.name != self.module
            || manifest.bus_name != self.bus_name
            || manifest.object_path != self.object_path
        {
            return Err(Error::Protocol(
                "required module identity does not match".into(),
            ));
        }
        for required in &self.interfaces {
            let matches = manifest.provides.iter().any(|provided| {
                provided.version.interface == required.name
                    && required.versions.accepts(&provided.version.speaks)
                    && required
                        .methods
                        .iter()
                        .all(|method| provided.methods.contains(method))
            });
            if !matches {
                return Err(Error::Protocol(
                    "required module interface is unavailable".into(),
                ));
            }
        }
        Ok(())
    }
}
