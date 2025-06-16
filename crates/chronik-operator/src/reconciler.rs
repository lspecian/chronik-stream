//! Reconciler for ChronikCluster resources.

use crate::crd::{ChronikCluster, ChronikClusterStatus, ClusterPhase, Condition, ReadyReplicas};
use crate::resources::ResourceGenerator;
use crate::finalizer::{FINALIZER_NAME, add_finalizer, remove_finalizer};
use chrono::Utc;
use k8s_openapi::{
    api::{
        apps::v1::{StatefulSet, Deployment},
        core::v1::{ConfigMap, Service, ServiceAccount, Secret},
        rbac::v1::{ClusterRole, ClusterRoleBinding, Role, RoleBinding},
        policy::v1::PodDisruptionBudget,
        autoscaling::v2::HorizontalPodAutoscaler,
    },
};
use kube::{
    api::{Api, Patch, PatchParams, PostParams, DeleteParams, ListParams},
    client::Client,
    runtime::controller::Action,
    Resource, ResourceExt,
};
use serde_json::json;
use std::time::Duration;
use tracing::{info, warn, error, debug};

/// Reconciler for ChronikCluster
pub struct Reconciler {
    client: Client,
}

impl Reconciler {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
    
    /// Main reconciliation loop
    pub async fn reconcile(&self, cluster: &ChronikCluster) -> Result<Action, kube::Error> {
        let name = cluster.metadata.name.as_ref().unwrap();
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let generation = cluster.metadata.generation.unwrap_or(0);
        
        info!("Reconciling ChronikCluster {}/{}", namespace, name);
        
        // Check if the resource is being deleted
        if cluster.metadata.deletion_timestamp.is_some() {
            return self.handle_deletion(cluster).await;
        }
        
        // Ensure finalizer is added
        if !cluster.finalizers().contains(&FINALIZER_NAME.to_string()) {
            let api: Api<ChronikCluster> = Api::namespaced(self.client.clone(), namespace);
            add_finalizer(&api, name).await?;
            return Ok(Action::requeue(Duration::from_secs(1)));
        }
        
        // Check if this is a new generation (spec changed)
        let status_generation = cluster.status.as_ref()
            .and_then(|s| s.observed_generation)
            .unwrap_or(0);
        
        if generation > status_generation {
            info!("Detected spec change for {}/{} (generation {} -> {})", 
                namespace, name, status_generation, generation);
        }
        
        // Update phase to Creating if it's Pending
        if matches!(cluster.status.as_ref().map(|s| &s.phase), Some(ClusterPhase::Pending) | None) {
            self.update_phase(cluster, ClusterPhase::Creating).await?;
        }
        
        // Create resource generator
        let generator = ResourceGenerator::new(cluster);
        
        // Reconcile RBAC resources
        self.reconcile_rbac(cluster).await?;
        
        // Reconcile ConfigMap
        self.reconcile_config_map(cluster, &generator).await?;
        
        // Reconcile controller resources
        self.reconcile_controller_resources(cluster, &generator).await?;
        
        // Reconcile ingest resources
        self.reconcile_ingest_resources(cluster, &generator).await?;
        
        // Reconcile search resources (if enabled)
        if cluster.spec.search_nodes.unwrap_or(0) > 0 {
            self.reconcile_search_resources(cluster, &generator).await?;
        }
        
        // Reconcile autoscaling resources
        self.reconcile_autoscaling(cluster, &generator).await?;
        
        // Update status
        self.update_status(cluster, generation).await?;
        
        // Requeue after 60 seconds for continuous reconciliation
        Ok(Action::requeue(Duration::from_secs(60)))
    }
    
    /// Handle resource deletion
    async fn handle_deletion(&self, cluster: &ChronikCluster) -> Result<Action, kube::Error> {
        let name = cluster.metadata.name.as_ref().unwrap();
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        info!("Handling deletion of ChronikCluster {}/{}", namespace, name);
        
        // Update phase to Deleting
        self.update_phase(cluster, ClusterPhase::Deleting).await?;
        
        // Clean up any external resources if needed
        // For now, Kubernetes will clean up owned resources automatically
        
        // Remove finalizer
        let api: Api<ChronikCluster> = Api::namespaced(self.client.clone(), namespace);
        remove_finalizer(&api, name).await?;
        
        Ok(Action::await_change())
    }
    
    /// Reconcile RBAC resources
    async fn reconcile_rbac(&self, cluster: &ChronikCluster) -> Result<(), kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let cluster_name = cluster.metadata.name.as_ref().unwrap();
        
        // Create service accounts
        for component in &["controller", "ingest", "search"] {
            let sa_name = format!("{}-{}", cluster_name, component);
            self.ensure_service_account(&sa_name, namespace, cluster).await?;
        }
        
        // Create roles and bindings
        self.ensure_rbac_rules(cluster).await?;
        
        Ok(())
    }
    
    /// Ensure service account exists
    async fn ensure_service_account(
        &self, 
        name: &str, 
        namespace: &str, 
        cluster: &ChronikCluster
    ) -> Result<(), kube::Error> {
        let api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), namespace);
        
        let sa = ServiceAccount {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                owner_references: Some(vec![self.owner_reference(cluster)]),
                ..Default::default()
            },
            ..Default::default()
        };
        
        match api.create(&PostParams::default(), &sa).await {
            Ok(_) => debug!("Created service account {}", name),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                debug!("Service account {} already exists", name);
            }
            Err(e) => return Err(e),
        }
        
        Ok(())
    }
    
    /// Ensure RBAC rules
    async fn ensure_rbac_rules(&self, cluster: &ChronikCluster) -> Result<(), kube::Error> {
        // This is a simplified version. In production, you'd create proper roles
        // with minimal required permissions for each component
        Ok(())
    }
    
    /// Reconcile ConfigMap
    async fn reconcile_config_map(
        &self, 
        cluster: &ChronikCluster,
        generator: &ResourceGenerator
    ) -> Result<(), kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let config_map = generator.config_map();
        let name = config_map.metadata.name.as_ref().unwrap();
        
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        
        match api.create(&PostParams::default(), &config_map).await {
            Ok(_) => info!("Created ConfigMap {}", name),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                // Update existing ConfigMap
                let patch_params = PatchParams::apply("chronik-operator").force();
                api.patch(name, &patch_params, &Patch::Apply(&config_map)).await?;
                debug!("Updated ConfigMap {}", name);
            }
            Err(e) => return Err(e),
        }
        
        Ok(())
    }
    
    /// Reconcile controller resources
    async fn reconcile_controller_resources(
        &self,
        cluster: &ChronikCluster,
        generator: &ResourceGenerator
    ) -> Result<(), kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        // StatefulSet
        let stateful_set = generator.controller_stateful_set();
        self.apply_stateful_set(namespace, stateful_set).await?;
        
        // Service
        let service = generator.controller_service();
        self.apply_service(namespace, service).await?;
        
        // PodDisruptionBudget
        if let Some(pdb) = generator.pod_disruption_budget("controller") {
            self.apply_pod_disruption_budget(namespace, pdb).await?;
        }
        
        Ok(())
    }
    
    /// Reconcile ingest resources
    async fn reconcile_ingest_resources(
        &self,
        cluster: &ChronikCluster,
        generator: &ResourceGenerator
    ) -> Result<(), kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        // StatefulSet
        let stateful_set = generator.ingest_stateful_set();
        self.apply_stateful_set(namespace, stateful_set).await?;
        
        // Service
        let service = generator.ingest_service();
        self.apply_service(namespace, service).await?;
        
        // PodDisruptionBudget
        if let Some(pdb) = generator.pod_disruption_budget("ingest") {
            self.apply_pod_disruption_budget(namespace, pdb).await?;
        }
        
        Ok(())
    }
    
    /// Reconcile search resources
    async fn reconcile_search_resources(
        &self,
        cluster: &ChronikCluster,
        generator: &ResourceGenerator
    ) -> Result<(), kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        // StatefulSet
        if let Some(stateful_set) = generator.search_stateful_set() {
            self.apply_stateful_set(namespace, stateful_set).await?;
        }
        
        // Service
        if let Some(service) = generator.search_service() {
            self.apply_service(namespace, service).await?;
        }
        
        // PodDisruptionBudget
        if let Some(pdb) = generator.pod_disruption_budget("search") {
            self.apply_pod_disruption_budget(namespace, pdb).await?;
        }
        
        Ok(())
    }
    
    /// Reconcile autoscaling resources
    async fn reconcile_autoscaling(
        &self,
        cluster: &ChronikCluster,
        generator: &ResourceGenerator
    ) -> Result<(), kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        // Controller HPA
        if let Some(hpa) = generator.horizontal_pod_autoscaler("controller") {
            self.apply_horizontal_pod_autoscaler(namespace, hpa).await?;
        }
        
        // Ingest HPA
        if let Some(hpa) = generator.horizontal_pod_autoscaler("ingest") {
            self.apply_horizontal_pod_autoscaler(namespace, hpa).await?;
        }
        
        // Search HPA
        if let Some(hpa) = generator.horizontal_pod_autoscaler("search") {
            self.apply_horizontal_pod_autoscaler(namespace, hpa).await?;
        }
        
        Ok(())
    }
    
    /// Apply StatefulSet
    async fn apply_stateful_set(
        &self,
        namespace: &str,
        stateful_set: StatefulSet
    ) -> Result<(), kube::Error> {
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), namespace);
        let name = stateful_set.metadata.name.as_ref().unwrap();
        
        match api.create(&PostParams::default(), &stateful_set).await {
            Ok(_) => info!("Created StatefulSet {}", name),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                // Update existing StatefulSet
                let patch_params = PatchParams::apply("chronik-operator").force();
                api.patch(name, &patch_params, &Patch::Apply(&stateful_set)).await?;
                debug!("Updated StatefulSet {}", name);
            }
            Err(e) => return Err(e),
        }
        
        Ok(())
    }
    
    /// Apply Service
    async fn apply_service(
        &self,
        namespace: &str,
        service: Service
    ) -> Result<(), kube::Error> {
        let api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let name = service.metadata.name.as_ref().unwrap();
        
        match api.create(&PostParams::default(), &service).await {
            Ok(_) => info!("Created Service {}", name),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                // Update existing Service
                let patch_params = PatchParams::apply("chronik-operator").force();
                api.patch(name, &patch_params, &Patch::Apply(&service)).await?;
                debug!("Updated Service {}", name);
            }
            Err(e) => return Err(e),
        }
        
        Ok(())
    }
    
    /// Apply PodDisruptionBudget
    async fn apply_pod_disruption_budget(
        &self,
        namespace: &str,
        pdb: PodDisruptionBudget
    ) -> Result<(), kube::Error> {
        let api: Api<PodDisruptionBudget> = Api::namespaced(self.client.clone(), namespace);
        let name = pdb.metadata.name.as_ref().unwrap();
        
        match api.create(&PostParams::default(), &pdb).await {
            Ok(_) => info!("Created PodDisruptionBudget {}", name),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                // Update existing PDB
                let patch_params = PatchParams::apply("chronik-operator").force();
                api.patch(name, &patch_params, &Patch::Apply(&pdb)).await?;
                debug!("Updated PodDisruptionBudget {}", name);
            }
            Err(e) => return Err(e),
        }
        
        Ok(())
    }
    
    /// Apply HorizontalPodAutoscaler
    async fn apply_horizontal_pod_autoscaler(
        &self,
        namespace: &str,
        hpa: HorizontalPodAutoscaler
    ) -> Result<(), kube::Error> {
        let api: Api<HorizontalPodAutoscaler> = Api::namespaced(self.client.clone(), namespace);
        let name = hpa.metadata.name.as_ref().unwrap();
        
        match api.create(&PostParams::default(), &hpa).await {
            Ok(_) => info!("Created HorizontalPodAutoscaler {}", name),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                // Update existing HPA
                let patch_params = PatchParams::apply("chronik-operator").force();
                api.patch(name, &patch_params, &Patch::Apply(&hpa)).await?;
                debug!("Updated HorizontalPodAutoscaler {}", name);
            }
            Err(e) => return Err(e),
        }
        
        Ok(())
    }
    
    /// Update cluster phase
    async fn update_phase(
        &self,
        cluster: &ChronikCluster,
        phase: ClusterPhase
    ) -> Result<(), kube::Error> {
        let name = cluster.metadata.name.as_ref().unwrap();
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        let api: Api<ChronikCluster> = Api::namespaced(self.client.clone(), namespace);
        
        let patch = json!({
            "status": {
                "phase": phase,
                "lastUpdated": Utc::now().to_rfc3339(),
            }
        });
        
        let patch_params = PatchParams::default();
        api.patch_status(name, &patch_params, &Patch::Merge(patch)).await?;
        
        Ok(())
    }
    
    /// Update cluster status
    async fn update_status(
        &self,
        cluster: &ChronikCluster,
        generation: i64
    ) -> Result<(), kube::Error> {
        let name = cluster.metadata.name.as_ref().unwrap();
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        
        // Get current state of resources
        let ready_replicas = self.get_ready_replicas(cluster).await?;
        let phase = self.determine_phase(cluster, &ready_replicas).await?;
        let conditions = self.build_conditions(cluster, &ready_replicas).await?;
        
        let status = ChronikClusterStatus {
            phase,
            controllers: self.get_controller_endpoints(cluster).await?,
            ingest_nodes: self.get_ingest_endpoints(cluster).await?,
            search_nodes: self.get_search_endpoints(cluster).await?,
            last_updated: Utc::now().to_rfc3339(),
            conditions,
            observed_generation: Some(generation),
            ready_replicas: Some(ready_replicas),
        };
        
        let api: Api<ChronikCluster> = Api::namespaced(self.client.clone(), namespace);
        let patch = json!({
            "status": status,
        });
        
        let patch_params = PatchParams::default();
        api.patch_status(name, &patch_params, &Patch::Merge(patch)).await?;
        
        Ok(())
    }
    
    /// Get ready replicas for all components
    async fn get_ready_replicas(&self, cluster: &ChronikCluster) -> Result<ReadyReplicas, kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let cluster_name = cluster.metadata.name.as_ref().unwrap();
        
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), namespace);
        
        // Get controller replicas
        let controller_name = format!("{}-controller", cluster_name);
        let controller_replicas = match api.get(&controller_name).await {
            Ok(sts) => sts.status.and_then(|s| s.ready_replicas).unwrap_or(0),
            Err(_) => 0,
        };
        
        // Get ingest replicas
        let ingest_name = format!("{}-ingest", cluster_name);
        let ingest_replicas = match api.get(&ingest_name).await {
            Ok(sts) => sts.status.and_then(|s| s.ready_replicas).unwrap_or(0),
            Err(_) => 0,
        };
        
        // Get search replicas
        let search_name = format!("{}-search", cluster_name);
        let search_replicas = match api.get(&search_name).await {
            Ok(sts) => sts.status.and_then(|s| s.ready_replicas).unwrap_or(0),
            Err(_) => 0,
        };
        
        Ok(ReadyReplicas {
            controllers: controller_replicas,
            ingest_nodes: ingest_replicas,
            search_nodes: search_replicas,
        })
    }
    
    /// Determine cluster phase based on current state
    async fn determine_phase(
        &self,
        cluster: &ChronikCluster,
        ready_replicas: &ReadyReplicas
    ) -> Result<ClusterPhase, kube::Error> {
        let expected_controllers = cluster.spec.controllers;
        let expected_ingest = cluster.spec.ingest_nodes;
        let expected_search = cluster.spec.search_nodes.unwrap_or(0);
        
        if ready_replicas.controllers == expected_controllers &&
           ready_replicas.ingest_nodes == expected_ingest &&
           ready_replicas.search_nodes == expected_search {
            Ok(ClusterPhase::Running)
        } else if ready_replicas.controllers > 0 || 
                  ready_replicas.ingest_nodes > 0 || 
                  ready_replicas.search_nodes > 0 {
            Ok(ClusterPhase::Updating)
        } else {
            Ok(ClusterPhase::Creating)
        }
    }
    
    /// Build conditions based on current state
    async fn build_conditions(
        &self,
        cluster: &ChronikCluster,
        ready_replicas: &ReadyReplicas
    ) -> Result<Vec<Condition>, kube::Error> {
        let mut conditions = Vec::new();
        
        // Controllers ready condition
        conditions.push(Condition {
            condition_type: "ControllersReady".to_string(),
            status: if ready_replicas.controllers == cluster.spec.controllers { "True" } else { "False" }.to_string(),
            last_transition_time: Utc::now().to_rfc3339(),
            reason: if ready_replicas.controllers == cluster.spec.controllers { 
                "AllControllersReady" 
            } else { 
                "ControllersNotReady" 
            }.to_string(),
            message: format!("{}/{} controllers ready", ready_replicas.controllers, cluster.spec.controllers),
        });
        
        // Ingest nodes ready condition
        conditions.push(Condition {
            condition_type: "IngestNodesReady".to_string(),
            status: if ready_replicas.ingest_nodes == cluster.spec.ingest_nodes { "True" } else { "False" }.to_string(),
            last_transition_time: Utc::now().to_rfc3339(),
            reason: if ready_replicas.ingest_nodes == cluster.spec.ingest_nodes { 
                "AllIngestNodesReady" 
            } else { 
                "IngestNodesNotReady" 
            }.to_string(),
            message: format!("{}/{} ingest nodes ready", ready_replicas.ingest_nodes, cluster.spec.ingest_nodes),
        });
        
        // Search nodes ready condition (if enabled)
        let expected_search = cluster.spec.search_nodes.unwrap_or(0);
        if expected_search > 0 {
            conditions.push(Condition {
                condition_type: "SearchNodesReady".to_string(),
                status: if ready_replicas.search_nodes == expected_search { "True" } else { "False" }.to_string(),
                last_transition_time: Utc::now().to_rfc3339(),
                reason: if ready_replicas.search_nodes == expected_search { 
                    "AllSearchNodesReady" 
                } else { 
                    "SearchNodesNotReady" 
                }.to_string(),
                message: format!("{}/{} search nodes ready", ready_replicas.search_nodes, expected_search),
            });
        }
        
        // Overall ready condition
        let all_ready = ready_replicas.controllers == cluster.spec.controllers &&
                       ready_replicas.ingest_nodes == cluster.spec.ingest_nodes &&
                       ready_replicas.search_nodes == expected_search;
        
        conditions.push(Condition {
            condition_type: "Ready".to_string(),
            status: if all_ready { "True" } else { "False" }.to_string(),
            last_transition_time: Utc::now().to_rfc3339(),
            reason: if all_ready { "AllComponentsReady" } else { "ComponentsNotReady" }.to_string(),
            message: if all_ready {
                "All Chronik Stream components are running".to_string()
            } else {
                "Some Chronik Stream components are not ready".to_string()
            },
        });
        
        Ok(conditions)
    }
    
    /// Get controller endpoints
    async fn get_controller_endpoints(&self, cluster: &ChronikCluster) -> Result<Vec<String>, kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let cluster_name = cluster.metadata.name.as_ref().unwrap();
        
        let ready_replicas = self.get_ready_replicas(cluster).await?;
        let mut endpoints = Vec::new();
        
        for i in 0..ready_replicas.controllers {
            endpoints.push(format!(
                "{}-controller-{}.{}-controller.{}.svc.cluster.local:9090",
                cluster_name, i, cluster_name, namespace
            ));
        }
        
        Ok(endpoints)
    }
    
    /// Get ingest endpoints
    async fn get_ingest_endpoints(&self, cluster: &ChronikCluster) -> Result<Vec<String>, kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let cluster_name = cluster.metadata.name.as_ref().unwrap();
        
        let ready_replicas = self.get_ready_replicas(cluster).await?;
        let mut endpoints = Vec::new();
        
        for i in 0..ready_replicas.ingest_nodes {
            endpoints.push(format!(
                "{}-ingest-{}.{}-ingest.{}.svc.cluster.local:9092",
                cluster_name, i, cluster_name, namespace
            ));
        }
        
        Ok(endpoints)
    }
    
    /// Get search endpoints
    async fn get_search_endpoints(&self, cluster: &ChronikCluster) -> Result<Vec<String>, kube::Error> {
        let namespace = cluster.metadata.namespace.as_ref().unwrap();
        let cluster_name = cluster.metadata.name.as_ref().unwrap();
        
        let ready_replicas = self.get_ready_replicas(cluster).await?;
        let mut endpoints = Vec::new();
        
        for i in 0..ready_replicas.search_nodes {
            endpoints.push(format!(
                "{}-search-{}.{}-search.{}.svc.cluster.local:9093",
                cluster_name, i, cluster_name, namespace
            ));
        }
        
        Ok(endpoints)
    }
    
    /// Create owner reference
    fn owner_reference(&self, cluster: &ChronikCluster) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: ChronikCluster::api_version(&()).to_string(),
            kind: ChronikCluster::kind(&()).to_string(),
            name: cluster.metadata.name.as_ref().unwrap().clone(),
            uid: cluster.metadata.uid.as_ref().unwrap().clone(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }
}