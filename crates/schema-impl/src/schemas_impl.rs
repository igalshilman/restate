// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use super::*;

use anyhow::anyhow;
use prost_reflect::{DescriptorPool, Kind, MethodDescriptor, ServiceDescriptor};
use proto_symbol::ProtoSymbols;
use restate_schema_api::discovery::KeyStructure;
use restate_schema_api::discovery::{
    DiscoveredInstanceType, FieldAnnotation, ServiceRegistrationRequest,
};
use restate_schema_api::service::InstanceType;
use restate_schema_api::subscription::{
    EventReceiverServiceInstanceType, FieldRemapType, InputEventRemap, Sink, Source,
};
use restate_types::identifiers::{EndpointId, ServiceRevision};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use tracing::{debug, info, warn};

const RESTATE_SERVICE_NAME_PREFIX: &str = "dev.restate.";
const GRPC_SERVICE_NAME_PREFIX: &str = "grpc.";

impl Schemas {
    pub(crate) fn use_service_schema<F, R>(&self, service_name: impl AsRef<str>, f: F) -> Option<R>
    where
        F: FnOnce(&ServiceSchemas) -> R,
    {
        let guard = self.0.load();
        guard.services.get(service_name.as_ref()).map(f)
    }
}

/// This struct contains the actual data held by Schemas.
#[derive(Debug, Clone)]
pub(crate) struct SchemasInner {
    pub(crate) services: HashMap<String, ServiceSchemas>,
    pub(crate) endpoints: HashMap<EndpointId, EndpointSchemas>,
    pub(crate) subscriptions: HashMap<String, Subscription>,
    pub(crate) proto_symbols: ProtoSymbols,
}

#[derive(Debug, Clone)]
pub(crate) struct MethodSchemas {
    descriptor: MethodDescriptor,
    input_fields_annotations: HashMap<FieldAnnotation, u32>,
}

impl MethodSchemas {
    pub(crate) fn new(
        descriptor: MethodDescriptor,
        input_fields_annotations: HashMap<FieldAnnotation, u32>,
    ) -> Self {
        Self {
            descriptor,
            input_fields_annotations,
        }
    }

    pub(crate) fn descriptor(&self) -> &MethodDescriptor {
        &self.descriptor
    }

    pub(crate) fn input_field_annotated(&self, annotation: FieldAnnotation) -> Option<u32> {
        self.input_fields_annotations.get(&annotation).cloned()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ServiceSchemas {
    pub(crate) revision: ServiceRevision,
    pub(crate) methods: HashMap<String, MethodSchemas>,
    pub(crate) instance_type: InstanceTypeMetadata,
    pub(crate) location: ServiceLocation,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum InstanceTypeMetadata {
    Keyed {
        key_structure: KeyStructure,
        service_methods_key_field_root_number: HashMap<String, u32>,
    },
    Unkeyed,
    Singleton,
    Unsupported,
    #[allow(dead_code)]
    Custom {
        // If method is missing, it means there's no key, hence a random key will be generated
        structure_per_method: HashMap<String, (u32, KeyStructure)>,
    },
}

impl InstanceTypeMetadata {
    pub(crate) fn keyed_with_scalar_key<'a>(
        methods: impl IntoIterator<Item = (&'a str, u32)>,
    ) -> Self {
        Self::Keyed {
            key_structure: KeyStructure::Scalar,
            service_methods_key_field_root_number: methods
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    pub(crate) fn from_discovered_metadata(
        instance_type: DiscoveredInstanceType,
        methods: &HashMap<String, DiscoveredMethodMetadata>,
    ) -> Self {
        match instance_type.clone() {
            DiscoveredInstanceType::Keyed(key_structure) => InstanceTypeMetadata::Keyed {
                key_structure,
                service_methods_key_field_root_number: methods
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            *v.input_fields_annotations
                                .get(&FieldAnnotation::Key)
                                .expect("At this point there must be a field annotated with key"),
                        )
                    })
                    .collect(),
            },
            DiscoveredInstanceType::Unkeyed => InstanceTypeMetadata::Unkeyed,
            DiscoveredInstanceType::Singleton => InstanceTypeMetadata::Singleton,
        }
    }
}

impl TryFrom<&InstanceTypeMetadata> for InstanceType {
    type Error = ();

    fn try_from(value: &InstanceTypeMetadata) -> Result<Self, Self::Error> {
        match value {
            InstanceTypeMetadata::Keyed { .. } => Ok(InstanceType::Keyed),
            InstanceTypeMetadata::Unkeyed => Ok(InstanceType::Unkeyed),
            InstanceTypeMetadata::Singleton => Ok(InstanceType::Singleton),
            _ => Err(()),
        }
    }
}

impl ServiceSchemas {
    fn new(
        revision: ServiceRevision,
        methods: HashMap<String, MethodSchemas>,
        instance_type: InstanceTypeMetadata,
        latest_endpoint: EndpointId,
    ) -> Self {
        Self {
            revision,
            methods,
            instance_type,
            location: ServiceLocation::ServiceEndpoint {
                latest_endpoint,
                public: true,
            },
        }
    }

    fn new_built_in(
        svc_desc: &ServiceDescriptor,
        instance_type: InstanceTypeMetadata,
        ingress_available: bool,
    ) -> Self {
        Self {
            revision: 0,
            methods: svc_desc
                .methods()
                .map(|descriptor| {
                    (
                        descriptor.name().to_string(),
                        MethodSchemas {
                            descriptor,
                            input_fields_annotations: Default::default(),
                        },
                    )
                })
                .collect(),
            instance_type,
            location: ServiceLocation::BuiltIn { ingress_available },
        }
    }

    fn compute_service_methods(
        svc_desc: &ServiceDescriptor,
        method_meta: &HashMap<String, DiscoveredMethodMetadata>,
    ) -> HashMap<String, MethodSchemas> {
        svc_desc
            .methods()
            .map(|descriptor| {
                let method_name = descriptor.name().to_string();
                let input_fields_annotations = method_meta
                    .get(&method_name)
                    .cloned()
                    .unwrap_or_default()
                    .input_fields_annotations;

                (
                    method_name,
                    MethodSchemas::new(descriptor, input_fields_annotations),
                )
            })
            .collect()
    }

    fn service_descriptor(&self) -> &ServiceDescriptor {
        self.methods
            .values()
            .next()
            .expect("A service should have at least one method")
            .descriptor
            .parent_service()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ServiceLocation {
    BuiltIn {
        // Available at the ingress
        ingress_available: bool,
    },
    ServiceEndpoint {
        // None if this is a built-in service
        latest_endpoint: EndpointId,
        public: bool,
    },
}

impl ServiceLocation {
    pub(crate) fn is_ingress_available(&self) -> bool {
        match self {
            ServiceLocation::BuiltIn {
                ingress_available, ..
            } => *ingress_available,
            ServiceLocation::ServiceEndpoint { public, .. } => *public,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EndpointSchemas {
    pub(crate) metadata: EndpointMetadata,
    pub(crate) services: Vec<(String, ServiceRevision)>,
}

impl Default for SchemasInner {
    fn default() -> Self {
        let mut inner = Self {
            services: Default::default(),
            endpoints: Default::default(),
            subscriptions: Default::default(),
            proto_symbols: Default::default(),
        };

        enum Visibility {
            Public,
            IngressAvailable,
            Internal,
        }

        // Register built-in services
        let mut register_built_in = |svc_name: &'static str,
                                     service_instance_type: InstanceTypeMetadata,
                                     visibility: Visibility| {
            inner.services.insert(
                svc_name.to_string(),
                ServiceSchemas::new_built_in(
                    &restate_pb::get_service(svc_name),
                    service_instance_type,
                    matches!(
                        visibility,
                        Visibility::Public | Visibility::IngressAvailable
                    ),
                ),
            );
            if matches!(visibility, Visibility::Public) {
                inner.proto_symbols.add_service(
                    &"self_ingress".to_string(),
                    &restate_pb::get_service(svc_name),
                )
            }
        };
        register_built_in(
            restate_pb::REFLECTION_SERVICE_NAME,
            InstanceTypeMetadata::Unsupported,
            Visibility::Public,
        );
        register_built_in(
            restate_pb::HEALTH_SERVICE_NAME,
            InstanceTypeMetadata::Unsupported,
            Visibility::Public,
        );
        register_built_in(
            restate_pb::INGRESS_SERVICE_NAME,
            InstanceTypeMetadata::Unkeyed,
            Visibility::Public,
        );
        register_built_in(
            restate_pb::AWAKEABLES_SERVICE_NAME,
            InstanceTypeMetadata::Unkeyed,
            Visibility::Public,
        );
        register_built_in(
            restate_pb::PROXY_SERVICE_NAME,
            // Key must be manually provided when invoking the proxy service
            InstanceTypeMetadata::Unsupported,
            Visibility::Internal,
        );
        register_built_in(
            restate_pb::REMOTE_CONTEXT_SERVICE_NAME,
            InstanceTypeMetadata::keyed_with_scalar_key([
                ("Start", 1),
                ("Send", 1),
                ("Recv", 1),
                ("GetResult", 1),
                ("Cleanup", 1),
            ]),
            Visibility::IngressAvailable,
        );
        register_built_in(
            restate_pb::IDEMPOTENT_INVOKER_SERVICE_NAME,
            InstanceTypeMetadata::Unsupported,
            Visibility::Internal,
        );

        inner
    }
}

impl SchemasInner {
    pub(crate) fn compute_new_endpoint_updates(
        &self,
        endpoint_metadata: EndpointMetadata,
        services: Vec<ServiceRegistrationRequest>,
        descriptor_pool: DescriptorPool,
        allow_overwrite: bool,
    ) -> Result<Vec<SchemasUpdateCommand>, RegistrationError> {
        let endpoint_id = endpoint_metadata.id();

        let mut result_commands = Vec::with_capacity(1 + services.len());

        if let Some(existing_endpoint) = self.endpoints.get(&endpoint_id) {
            if allow_overwrite {
                // If we need to overwrite the endpoint we need to remove old services
                for (svc_name, revision) in &existing_endpoint.services {
                    warn!(
                        restate.service_endpoint.id = %endpoint_id,
                        restate.service_endpoint.address = %endpoint_metadata.address_display(),
                        "Going to remove service {} due to a forced service endpoint update",
                        svc_name
                    );
                    result_commands.push(SchemasUpdateCommand::RemoveService {
                        name: svc_name.to_string(),
                        revision: *revision,
                    });
                }
            } else {
                return Err(RegistrationError::OverrideEndpoint(endpoint_id));
            }
        }

        // Compute service revision numbers
        let mut computed_revisions = HashMap::with_capacity(services.len());
        for service_meta in &services {
            check_is_reserved(&service_meta.name)?;

            let instance_type = InstanceTypeMetadata::from_discovered_metadata(
                service_meta.instance_type.clone(),
                &service_meta.methods,
            );

            // For the time being when updating we overwrite existing data
            let revision = if let Some(service_schemas) = self.services.get(&service_meta.name) {
                // Check instance type
                if service_schemas.instance_type != instance_type {
                    if allow_overwrite {
                        warn!(
                            restate.service_endpoint.id = %endpoint_id,
                            restate.service_endpoint.address = %endpoint_metadata.address_display(),
                            "Going to overwrite service instance type {} due to a forced service endpoint update: {:?} != {:?}. This is a potentially dangerous operation, and might result in data loss.",
                            service_meta.name,
                            service_schemas.instance_type,
                            instance_type
                        );
                    } else {
                        return Err(RegistrationError::DifferentServiceInstanceType(
                            service_meta.name.clone(),
                        ));
                    }
                }

                service_schemas.revision.wrapping_add(1)
            } else {
                1
            };
            computed_revisions.insert(service_meta.name.clone(), revision);
        }

        // Create the InsertEndpoint command
        result_commands.push(SchemasUpdateCommand::InsertEndpoint {
            metadata: endpoint_metadata,
            services: services
                .into_iter()
                .map(|service_meta| {
                    let revision = computed_revisions.remove(&service_meta.name).unwrap();

                    InsertServiceUpdateCommand {
                        name: service_meta.name,
                        revision,
                        instance_type: service_meta.instance_type,
                        methods: service_meta.methods,
                    }
                })
                .collect(),
            descriptor_pool,
        });

        Ok(result_commands)
    }

    pub(crate) fn compute_modify_service_updates(
        &self,
        name: String,
        public: bool,
    ) -> Result<SchemasUpdateCommand, RegistrationError> {
        check_is_reserved(&name)?;
        if !self.services.contains_key(&name) {
            return Err(RegistrationError::UnknownService(name));
        }

        Ok(SchemasUpdateCommand::ModifyService { name, public })
    }

    pub(crate) fn compute_remove_endpoint(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<Vec<SchemasUpdateCommand>, RegistrationError> {
        if !self.endpoints.contains_key(&endpoint_id) {
            return Err(RegistrationError::UnknownEndpoint(endpoint_id));
        }
        let endpoint_schemas = self.endpoints.get(&endpoint_id).unwrap();

        let mut commands = Vec::with_capacity(1 + endpoint_schemas.services.len());
        for (name, revision) in endpoint_schemas.services.clone() {
            commands.push(SchemasUpdateCommand::RemoveService { name, revision });
        }
        commands.push(SchemasUpdateCommand::RemoveEndpoint { endpoint_id });

        Ok(commands)
    }

    pub(crate) fn compute_add_subscription<V: SubscriptionValidator>(
        &self,
        id: Option<String>,
        source: Uri,
        sink: Uri,
        metadata: Option<HashMap<String, String>>,
        validator: V,
    ) -> Result<(Subscription, SchemasUpdateCommand), RegistrationError> {
        // TODO We could generate a more human readable uuid here by taking the source and sink,
        // and adding an incremental number in case of collision.
        let id = id.unwrap_or_else(|| uuid::Uuid::now_v7().as_simple().to_string());

        if self.subscriptions.contains_key(&id) {
            return Err(RegistrationError::OverrideSubscription(id));
        }

        // TODO This logic to parse source and sink should be moved elsewhere to abstract over the known source/sink providers
        //  Maybe together with the validator?

        // Parse source
        let source = match source.scheme_str() {
            Some("kafka") => {
                let cluster_name = source.authority().ok_or_else(|| RegistrationError::InvalidSubscription(anyhow!(
                    "source URI of Kafka type must have a authority segment containing the cluster name. Was '{}'",
                    source
                )))?.as_str();
                let topic_name = &source.path()[1..];
                Source::Kafka {
                    cluster: cluster_name.to_string(),
                    topic: topic_name.to_string(),
                    ordering_key_format: Default::default(),
                }
            }
            _ => {
                return Err(RegistrationError::InvalidSubscription(anyhow!(
                    "source URI must have a scheme segment, with supported schemes: {:?}. Was '{}'",
                    ["kafka"],
                    source
                )))
            }
        };

        // Parse sink
        let sink = match sink.scheme_str() {
            Some("service") => {
                let service_name = sink.authority().ok_or_else(|| RegistrationError::InvalidSubscription(anyhow!(
                    "sink URI of service type must have a authority segment containing the service name. Was '{}'",
                    sink
                )))?.as_str();
                let method_name = &sink.path()[1..];

                // Retrieve service and method in the schema registry
                let service_schemas = self.services.get(service_name).ok_or_else(|| {
                    RegistrationError::InvalidSubscription(anyhow!(
                        "cannot find service specified in the sink URI. Was '{}'",
                        sink
                    ))
                })?;
                let method_schemas = service_schemas.methods.get(method_name).ok_or_else(|| {
                    RegistrationError::InvalidSubscription(anyhow!(
                        "cannot find service method specified in the sink URI. Was '{}'",
                        sink
                    ))
                })?;

                let input_type = method_schemas.descriptor().input();
                let input_event_remap = if input_type.full_name() == "dev.restate.Event" {
                    // No remapping needed
                    None
                } else {
                    let key = if let Some(index) =
                        method_schemas.input_field_annotated(FieldAnnotation::Key)
                    {
                        let kind = input_type.get_field(index).unwrap().kind();
                        if kind == Kind::String {
                            Some((index, FieldRemapType::String))
                        } else {
                            Some((index, FieldRemapType::Bytes))
                        }
                    } else {
                        None
                    };

                    let payload = if let Some(index) =
                        method_schemas.input_field_annotated(FieldAnnotation::EventPayload)
                    {
                        let kind = input_type.get_field(index).unwrap().kind();
                        if kind == Kind::String {
                            Some((index, FieldRemapType::String))
                        } else {
                            Some((index, FieldRemapType::Bytes))
                        }
                    } else {
                        None
                    };

                    Some(InputEventRemap {
                        key,
                        payload,
                        attributes_index: method_schemas
                            .input_field_annotated(FieldAnnotation::EventMetadata),
                    })
                };

                let instance_type = match service_schemas.instance_type {
                    InstanceTypeMetadata::Keyed { .. } => {
                        // Verify the type is supported!
                        let key_field_kind = method_schemas.descriptor.input().get_field(
                            method_schemas.input_field_annotated(FieldAnnotation::Key).expect("There must be a key field for every method input type")
                        ).unwrap().kind();
                        if key_field_kind != Kind::String && key_field_kind != Kind::Bytes {
                            return Err(RegistrationError::InvalidSubscription(anyhow!(
                                "Key type {:?} for sink {} is invalid, only bytes and string are supported.",
                                key_field_kind, sink
                            )));
                        }

                        EventReceiverServiceInstanceType::Keyed { ordering_key_is_key: false }
                    }
                    InstanceTypeMetadata::Unkeyed => EventReceiverServiceInstanceType::Unkeyed,
                    InstanceTypeMetadata::Singleton => EventReceiverServiceInstanceType::Singleton,
                    InstanceTypeMetadata::Unsupported | InstanceTypeMetadata::Custom { .. } => {
                        return Err(RegistrationError::InvalidSubscription(anyhow!(
                            "trying to use a built-in service as sink {}. This is currently unsupported.",
                            sink
                        )))
                    }
                };

                Sink::Service {
                    name: service_name.to_string(),
                    method: method_name.to_string(),
                    input_event_remap,
                    instance_type,
                }
            }
            _ => {
                return Err(RegistrationError::InvalidSubscription(anyhow!(
                    "sink URI must have a scheme segment, with supported schemes: {:?}. Was '{}'",
                    ["service"],
                    sink
                )))
            }
        };

        let subscription = validator
            .validate(Subscription::new(
                id,
                source,
                sink,
                metadata.unwrap_or_default(),
            ))
            .map_err(|e| RegistrationError::InvalidSubscription(e.into()))?;

        Ok((
            subscription.clone(),
            SchemasUpdateCommand::AddSubscription(subscription),
        ))
    }

    pub(crate) fn compute_remove_subscription(
        &self,
        id: String,
    ) -> Result<SchemasUpdateCommand, RegistrationError> {
        if !self.subscriptions.contains_key(&id) {
            return Err(RegistrationError::UnknownSubscription(id));
        }

        Ok(SchemasUpdateCommand::RemoveSubscription(id))
    }

    pub(crate) fn apply_update(
        &mut self,
        update_cmd: SchemasUpdateCommand,
    ) -> Result<(), RegistrationError> {
        match update_cmd {
            SchemasUpdateCommand::InsertEndpoint {
                metadata,
                services,
                descriptor_pool,
            } => {
                let endpoint_id = metadata.id();
                info!(
                    restate.service_endpoint.id = %endpoint_id,
                    restate.service_endpoint.address = %metadata.address_display(),
                    "Registering endpoint"
                );

                let mut endpoint_services = vec![];

                for InsertServiceUpdateCommand {
                    name,
                    revision,
                    instance_type,
                    methods,
                } in services
                {
                    info!(
                        rpc.service = name,
                        restate.service_endpoint.address = %metadata.address_display(),
                        "Registering service"
                    );
                    let service_descriptor =
                        descriptor_pool.get_service_by_name(&name).ok_or_else(|| {
                            RegistrationError::MissingServiceInDescriptor(name.clone())
                        })?;

                    if tracing::enabled!(tracing::Level::DEBUG) {
                        service_descriptor.methods().for_each(|method| {
                            debug!(
                                rpc.service = name,
                                rpc.method = method.name(),
                                "Registering method"
                            )
                        });
                    }

                    // We need to retain the `public` field from previous registrations
                    self.services
                        .entry(name.clone())
                        .and_modify(|service_schemas| {
                            info!(rpc.service = name, "Overwriting existing service schemas");

                            service_schemas.revision = revision;
                            service_schemas.instance_type =
                                InstanceTypeMetadata::from_discovered_metadata(
                                    instance_type.clone(),
                                    &methods,
                                );
                            service_schemas.methods = ServiceSchemas::compute_service_methods(
                                &service_descriptor,
                                &methods,
                            );
                            if let ServiceLocation::ServiceEndpoint {
                                latest_endpoint, ..
                            } = &mut service_schemas.location
                            {
                                *latest_endpoint = endpoint_id.clone();
                            }

                            // We need to remove the service from the proto_symbols.
                            // We re-insert it later with the new endpoint id
                            self.proto_symbols.remove_service(&service_descriptor);
                        })
                        .or_insert_with(|| {
                            ServiceSchemas::new(
                                revision,
                                ServiceSchemas::compute_service_methods(
                                    &service_descriptor,
                                    &methods,
                                ),
                                InstanceTypeMetadata::from_discovered_metadata(
                                    instance_type.clone(),
                                    &methods,
                                ),
                                endpoint_id.clone(),
                            )
                        });

                    self.proto_symbols
                        .add_service(&endpoint_id, &service_descriptor);

                    endpoint_services.push((name, revision));
                }

                self.endpoints.insert(
                    endpoint_id,
                    EndpointSchemas {
                        metadata,
                        services: endpoint_services,
                    },
                );
            }
            SchemasUpdateCommand::RemoveEndpoint { endpoint_id } => {
                self.endpoints.remove(&endpoint_id);
            }
            SchemasUpdateCommand::RemoveService { name, revision } => {
                let entry = self.services.entry(name);
                match entry {
                    Entry::Occupied(e) if e.get().revision == revision => {
                        let schemas = e.remove();
                        self.proto_symbols
                            .remove_service(schemas.service_descriptor());
                    }
                    _ => {}
                }
            }
            SchemasUpdateCommand::ModifyService {
                name,
                public: new_public_value,
            } => {
                let schemas = self
                    .services
                    .get_mut(&name)
                    .ok_or_else(|| RegistrationError::UnknownService(name.clone()))?;

                // Update proto_symbols
                if let ServiceLocation::ServiceEndpoint {
                    public: old_public_value,
                    latest_endpoint,
                } = &schemas.location
                {
                    match (*old_public_value, new_public_value) {
                        (true, false) => {
                            self.proto_symbols
                                .remove_service(schemas.service_descriptor());
                        }
                        (false, true) => {
                            self.proto_symbols
                                .add_service(latest_endpoint, schemas.service_descriptor());
                        }
                        _ => {}
                    }
                }

                // Update the public field
                if let ServiceLocation::ServiceEndpoint {
                    public: old_public_value,
                    ..
                } = &mut schemas.location
                {
                    *old_public_value = new_public_value;
                }
            }
            SchemasUpdateCommand::AddSubscription(sub) => {
                self.subscriptions.insert(sub.id().to_string(), sub);
            }
            SchemasUpdateCommand::RemoveSubscription(sub_id) => {
                self.subscriptions.remove(&sub_id);
            }
        }

        Ok(())
    }
}

fn check_is_reserved(svc_name: &str) -> Result<(), RegistrationError> {
    if svc_name.starts_with(GRPC_SERVICE_NAME_PREFIX)
        || svc_name.starts_with(RESTATE_SERVICE_NAME_PREFIX)
    {
        return Err(RegistrationError::ModifyInternalService(
            svc_name.to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use restate_pb::mocks;
    use restate_schema_api::endpoint::EndpointMetadataResolver;
    use restate_schema_api::service::ServiceMetadataResolver;
    use restate_test_util::{assert, assert_eq, let_assert, test};

    impl Schemas {
        fn assert_service_revision(&self, svc_name: &str, revision: ServiceRevision) {
            assert_eq!(
                self.resolve_latest_service_metadata(svc_name)
                    .unwrap()
                    .revision,
                revision
            );
        }

        fn assert_resolves_endpoint(&self, svc_name: &str, endpoint_id: EndpointId) {
            assert_eq!(
                self.resolve_latest_endpoint_for_service(svc_name)
                    .unwrap()
                    .id(),
                endpoint_id
            );
        }
    }

    #[test]
    fn register_new_endpoint_empty_registry() {
        let schemas = Schemas::default();

        let endpoint = EndpointMetadata::mock();
        let commands = schemas
            .compute_new_endpoint_updates(
                endpoint.clone(),
                vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                    mocks::GREETER_SERVICE_NAME.to_string(),
                    &["Greet"],
                )],
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();

        let_assert!(Some(SchemasUpdateCommand::InsertEndpoint { services, .. }) = commands.get(0));
        assert_eq!(services.len(), 1);

        schemas.apply_updates(commands).unwrap();

        schemas.assert_service_revision(mocks::GREETER_SERVICE_NAME, 1);
        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint.id());
    }

    #[test]
    fn register_new_endpoint_updating_old_service() {
        let schemas = Schemas::default();

        let endpoint_1 = EndpointMetadata::mock_with_uri("http://localhost:9080");
        let endpoint_2 = EndpointMetadata::mock_with_uri("http://localhost:9081");

        let commands = schemas
            .compute_new_endpoint_updates(
                endpoint_1.clone(),
                vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                    mocks::GREETER_SERVICE_NAME.to_string(),
                    &["Greet"],
                )],
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();

        let_assert!(Some(SchemasUpdateCommand::InsertEndpoint { services, .. }) = commands.get(0));
        assert_eq!(services.len(), 1);

        schemas.apply_updates(commands).unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint_1.id());

        let commands = schemas
            .compute_new_endpoint_updates(
                endpoint_2.clone(),
                vec![
                    ServiceRegistrationRequest::unkeyed_without_annotations(
                        mocks::GREETER_SERVICE_NAME.to_string(),
                        &["Greet"],
                    ),
                    ServiceRegistrationRequest::unkeyed_without_annotations(
                        mocks::ANOTHER_GREETER_SERVICE_NAME.to_string(),
                        &["Greet"],
                    ),
                ],
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();

        let_assert!(Some(SchemasUpdateCommand::InsertEndpoint { services, .. }) = commands.get(0));
        assert_eq!(services.len(), 2);

        schemas.apply_updates(commands).unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint_2.id());
        schemas.assert_service_revision(mocks::GREETER_SERVICE_NAME, 2);
        schemas.assert_resolves_endpoint(mocks::ANOTHER_GREETER_SERVICE_NAME, endpoint_2.id());
        schemas.assert_service_revision(mocks::ANOTHER_GREETER_SERVICE_NAME, 1);
    }

    #[test]
    fn register_new_endpoint_updating_old_service_fails_with_different_instance_type() {
        let schemas = Schemas::default();

        let endpoint_1 = EndpointMetadata::mock_with_uri("http://localhost:9080");
        let endpoint_2 = EndpointMetadata::mock_with_uri("http://localhost:9081");

        schemas
            .apply_updates(
                schemas
                    .compute_new_endpoint_updates(
                        endpoint_1.clone(),
                        vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                            mocks::GREETER_SERVICE_NAME.to_string(),
                            &["Greet"],
                        )],
                        mocks::DESCRIPTOR_POOL.clone(),
                        false,
                    )
                    .unwrap(),
            )
            .unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint_1.id());

        let compute_result = schemas.compute_new_endpoint_updates(
            endpoint_2,
            vec![ServiceRegistrationRequest::singleton_without_annotations(
                mocks::GREETER_SERVICE_NAME.to_string(),
                &["Greet"],
            )],
            mocks::DESCRIPTOR_POOL.clone(),
            false,
        );

        assert!(let Err(RegistrationError::DifferentServiceInstanceType(_)) = compute_result);
    }

    #[test]
    fn override_existing_endpoint() {
        let schemas = Schemas::default();

        let endpoint = EndpointMetadata::mock();
        let commands = schemas
            .compute_new_endpoint_updates(
                endpoint.clone(),
                vec![
                    ServiceRegistrationRequest::unkeyed_without_annotations(
                        mocks::GREETER_SERVICE_NAME.to_string(),
                        &["Greet"],
                    ),
                    ServiceRegistrationRequest::unkeyed_without_annotations(
                        mocks::ANOTHER_GREETER_SERVICE_NAME.to_string(),
                        &["Greet"],
                    ),
                ],
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();
        schemas.apply_updates(commands).unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint.id());
        schemas.assert_resolves_endpoint(mocks::ANOTHER_GREETER_SERVICE_NAME, endpoint.id());

        let commands = schemas
            .compute_new_endpoint_updates(
                endpoint.clone(),
                vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                    mocks::GREETER_SERVICE_NAME.to_string(),
                    &["Greet"],
                )],
                mocks::DESCRIPTOR_POOL.clone(),
                true,
            )
            .unwrap();
        schemas.apply_updates(commands).unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint.id());
        assert!(schemas
            .resolve_latest_endpoint_for_service(mocks::ANOTHER_GREETER_SERVICE_NAME)
            .is_none());
    }

    #[test]
    fn cannot_override_existing_endpoint() {
        let schemas = Schemas::default();

        let endpoint = EndpointMetadata::mock();
        let services = vec![ServiceRegistrationRequest::unkeyed_without_annotations(
            mocks::GREETER_SERVICE_NAME.to_string(),
            &["Greet"],
        )];

        let commands = schemas
            .compute_new_endpoint_updates(
                endpoint.clone(),
                services.clone(),
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();
        schemas.apply_updates(commands).unwrap();

        assert!(let Err(RegistrationError::OverrideEndpoint(_)) = schemas.compute_new_endpoint_updates(endpoint, services, mocks::DESCRIPTOR_POOL.clone(), false));
    }

    #[test]
    fn register_two_endpoints_then_remove_first() {
        let schemas = Schemas::default();

        let endpoint_1 = EndpointMetadata::mock_with_uri("http://localhost:9080");
        let endpoint_2 = EndpointMetadata::mock_with_uri("http://localhost:9081");

        schemas
            .apply_updates(
                schemas
                    .compute_new_endpoint_updates(
                        endpoint_1.clone(),
                        vec![
                            ServiceRegistrationRequest::unkeyed_without_annotations(
                                mocks::GREETER_SERVICE_NAME.to_string(),
                                &["Greet"],
                            ),
                            ServiceRegistrationRequest::unkeyed_without_annotations(
                                mocks::ANOTHER_GREETER_SERVICE_NAME.to_string(),
                                &["Greet"],
                            ),
                        ],
                        mocks::DESCRIPTOR_POOL.clone(),
                        false,
                    )
                    .unwrap(),
            )
            .unwrap();
        schemas
            .apply_updates(
                schemas
                    .compute_new_endpoint_updates(
                        endpoint_2.clone(),
                        vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                            mocks::GREETER_SERVICE_NAME.to_string(),
                            &["Greet"],
                        )],
                        mocks::DESCRIPTOR_POOL.clone(),
                        false,
                    )
                    .unwrap(),
            )
            .unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint_2.id());
        schemas.assert_service_revision(mocks::GREETER_SERVICE_NAME, 2);
        schemas.assert_resolves_endpoint(mocks::ANOTHER_GREETER_SERVICE_NAME, endpoint_1.id());
        schemas.assert_service_revision(mocks::ANOTHER_GREETER_SERVICE_NAME, 1);

        let commands = schemas.compute_remove_endpoint(endpoint_1.id()).unwrap();

        assert!(
            let Some(SchemasUpdateCommand::RemoveService { .. }) = commands.get(0)
        );
        assert!(
            let Some(SchemasUpdateCommand::RemoveService { .. }) = commands.get(1)
        );
        assert!(
            let Some(SchemasUpdateCommand::RemoveEndpoint { .. }) = commands.get(2)
        );

        schemas.apply_updates(commands).unwrap();

        schemas.assert_resolves_endpoint(mocks::GREETER_SERVICE_NAME, endpoint_2.id());
        schemas.assert_service_revision(mocks::GREETER_SERVICE_NAME, 2);
        assert!(schemas
            .resolve_latest_endpoint_for_service(mocks::ANOTHER_GREETER_SERVICE_NAME)
            .is_none());
        assert!(schemas.get_endpoint(&endpoint_1.id()).is_none());
    }

    // Reproducer for issue where the service name is the same of the method name
    #[test]
    fn register_issue682() {
        let schemas = Schemas::default();
        let svc_name = "test.Issue682";

        let endpoint = EndpointMetadata::mock();
        schemas
            .apply_updates(
                schemas
                    .compute_new_endpoint_updates(
                        endpoint.clone(),
                        vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                            svc_name.to_string(),
                            &["Greet"],
                        )],
                        mocks::DESCRIPTOR_POOL.clone(),
                        false,
                    )
                    .unwrap(),
            )
            .unwrap();
        schemas.assert_service_revision(svc_name, 1);

        // Force the update. This should not panic.
        schemas
            .apply_updates(
                schemas
                    .compute_new_endpoint_updates(
                        endpoint,
                        vec![ServiceRegistrationRequest::unkeyed_without_annotations(
                            svc_name.to_string(),
                            &["Greet"],
                        )],
                        mocks::DESCRIPTOR_POOL.clone(),
                        true,
                    )
                    .unwrap(),
            )
            .unwrap();
        schemas.assert_service_revision(svc_name, 2);
    }

    #[test]
    fn proto_list_service_should_not_contain_remote_context() {
        let schemas = Schemas::default();
        assert!(
            !restate_schema_api::proto_symbol::ProtoSymbolResolver::list_services(&schemas)
                .contains(&"dev.restate.internal.RemoteContext".to_string())
        );
    }
}
