//! Generate CRD YAML for ChronikCluster

use kube::CustomResourceExt;

// Re-export the CRD from the lib
use chronik_operator::crd::ChronikCluster;

fn main() {
    let crd = ChronikCluster::crd();
    println!("{}", serde_yaml::to_string(&crd).unwrap());
}