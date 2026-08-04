//! Service registration and dependency ordering for the log-to-training pipeline.

use std::collections::{HashMap, HashSet};

/// An orchestrator-managed component in the log-to-training pipeline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Service {
    FleetLogShipper,
    FleetLogDigest,
    FfInteractions,
    TrainingDataExport,
}

impl Service {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FleetLogShipper => "fleet_log_shipper",
            Self::FleetLogDigest => "fleet_log_digest",
            Self::FfInteractions => "ff_interactions",
            Self::TrainingDataExport => "training_data_export",
        }
    }
}

/// The kind of data exchanged by two registered services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataKind {
    RawLogs,
    LogDigest,
    Interaction,
    TrainingData,
}

/// A typed data flow between registered services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataFlow {
    pub source: Service,
    pub destination: Service,
    pub data: DataKind,
}

/// One orchestrator-managed service and the services that must start before it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceRegistration {
    pub service: Service,
    pub dependencies: Vec<Service>,
}

/// A validated registration manifest for a coordinated service pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceManifest {
    pub services: Vec<ServiceRegistration>,
    pub flows: Vec<DataFlow>,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum RegistrationError {
    #[error("duplicate service registration: {}", .0.as_str())]
    DuplicateService(Service),
    #[error(
        "service {} depends on unregistered service {}",
        .service.as_str(),
        .dependency.as_str()
    )]
    MissingDependency {
        service: Service,
        dependency: Service,
    },
    #[error("data flow references unregistered service: {}", .0.as_str())]
    MissingFlowService(Service),
    #[error("service dependency cycle includes: {}", .0.as_str())]
    DependencyCycle(Service),
}

impl ServiceManifest {
    /// Validate registrations and return services in dependency-first order.
    ///
    /// Data-flow sources are implicit dependencies of their destinations, so a
    /// consumer cannot start before its producer even if the explicit
    /// registration omits that relationship.
    pub fn startup_order(&self) -> Result<Vec<Service>, RegistrationError> {
        let mut registrations = HashMap::new();
        for registration in &self.services {
            if registrations
                .insert(registration.service, registration)
                .is_some()
            {
                return Err(RegistrationError::DuplicateService(registration.service));
            }
        }

        let mut dependencies: HashMap<Service, Vec<Service>> = self
            .services
            .iter()
            .map(|registration| (registration.service, registration.dependencies.clone()))
            .collect();

        for registration in &self.services {
            for dependency in &registration.dependencies {
                if !registrations.contains_key(dependency) {
                    return Err(RegistrationError::MissingDependency {
                        service: registration.service,
                        dependency: *dependency,
                    });
                }
            }
        }
        for flow in &self.flows {
            for service in [flow.source, flow.destination] {
                if !registrations.contains_key(&service) {
                    return Err(RegistrationError::MissingFlowService(service));
                }
            }
            let destination_dependencies = &mut dependencies
                .get_mut(&flow.destination)
                .expect("flow destination was validated");
            if !destination_dependencies.contains(&flow.source) {
                destination_dependencies.push(flow.source);
            }
        }

        fn visit(
            service: Service,
            dependencies: &HashMap<Service, Vec<Service>>,
            visiting: &mut HashSet<Service>,
            visited: &mut HashSet<Service>,
            ordered: &mut Vec<Service>,
        ) -> Result<(), RegistrationError> {
            if visited.contains(&service) {
                return Ok(());
            }
            if !visiting.insert(service) {
                return Err(RegistrationError::DependencyCycle(service));
            }
            for dependency in &dependencies[&service] {
                visit(*dependency, dependencies, visiting, visited, ordered)?;
            }
            visiting.remove(&service);
            visited.insert(service);
            ordered.push(service);
            Ok(())
        }

        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        let mut ordered = Vec::with_capacity(self.services.len());
        for registration in &self.services {
            visit(
                registration.service,
                &dependencies,
                &mut visiting,
                &mut visited,
                &mut ordered,
            )?;
        }
        Ok(ordered)
    }
}

/// Canonical log shipping and training-data service registrations.
pub fn log_shipping_manifest() -> ServiceManifest {
    ServiceManifest {
        services: vec![
            ServiceRegistration {
                service: Service::FleetLogShipper,
                dependencies: vec![],
            },
            ServiceRegistration {
                service: Service::FleetLogDigest,
                dependencies: vec![Service::FleetLogShipper],
            },
            ServiceRegistration {
                service: Service::FfInteractions,
                dependencies: vec![Service::FleetLogShipper],
            },
            ServiceRegistration {
                service: Service::TrainingDataExport,
                dependencies: vec![Service::FleetLogDigest, Service::FfInteractions],
            },
        ],
        flows: vec![
            DataFlow {
                source: Service::FleetLogShipper,
                destination: Service::FleetLogDigest,
                data: DataKind::RawLogs,
            },
            DataFlow {
                source: Service::FleetLogShipper,
                destination: Service::FfInteractions,
                data: DataKind::Interaction,
            },
            DataFlow {
                source: Service::FleetLogDigest,
                destination: Service::TrainingDataExport,
                data: DataKind::LogDigest,
            },
            DataFlow {
                source: Service::FfInteractions,
                destination: Service::TrainingDataExport,
                data: DataKind::TrainingData,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_shipping_services_start_in_dependency_order() {
        let manifest = log_shipping_manifest();
        assert_eq!(
            manifest.startup_order().unwrap(),
            [
                Service::FleetLogShipper,
                Service::FleetLogDigest,
                Service::FfInteractions,
                Service::TrainingDataExport
            ]
        );
    }

    #[test]
    fn data_flow_imposes_startup_dependency() {
        let manifest = ServiceManifest {
            services: vec![
                ServiceRegistration {
                    service: Service::TrainingDataExport,
                    dependencies: vec![],
                },
                ServiceRegistration {
                    service: Service::FfInteractions,
                    dependencies: vec![],
                },
            ],
            flows: vec![DataFlow {
                source: Service::FfInteractions,
                destination: Service::TrainingDataExport,
                data: DataKind::TrainingData,
            }],
        };
        assert_eq!(
            manifest.startup_order().unwrap(),
            [Service::FfInteractions, Service::TrainingDataExport]
        );
    }

    #[test]
    fn registration_rejects_missing_dependencies() {
        let manifest = ServiceManifest {
            services: vec![ServiceRegistration {
                service: Service::TrainingDataExport,
                dependencies: vec![Service::FfInteractions],
            }],
            flows: vec![],
        };
        assert_eq!(
            manifest.startup_order(),
            Err(RegistrationError::MissingDependency {
                service: Service::TrainingDataExport,
                dependency: Service::FfInteractions,
            })
        );
    }

    #[test]
    fn registration_rejects_dependency_cycles() {
        let manifest = ServiceManifest {
            services: vec![
                ServiceRegistration {
                    service: Service::FleetLogDigest,
                    dependencies: vec![Service::FleetLogShipper],
                },
                ServiceRegistration {
                    service: Service::FleetLogShipper,
                    dependencies: vec![Service::FleetLogDigest],
                },
            ],
            flows: vec![],
        };
        assert!(matches!(
            manifest.startup_order(),
            Err(RegistrationError::DependencyCycle(_))
        ));
    }
}
