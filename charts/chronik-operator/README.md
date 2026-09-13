# chronik-operator

Kubernetes operator for Chronik Stream. Manages `ChronikCluster`,
`ChronikStandalone`, `ChronikTopic`, `ChronikUser` and `ChronikAutoScaler`.

## Install

```bash
helm install chronik-operator charts/chronik-operator -n chronik-system --create-namespace
```

## Upgrade

```bash
# 1. CRDs first — Helm will NOT do this for you (see below).
kubectl apply --server-side -f charts/chronik-operator/crds/

# 2. Then the operator itself.
helm upgrade chronik-operator charts/chronik-operator -n chronik-system
```

### Helm does not upgrade CRDs

Everything under `crds/` is applied **only on first install**. `helm upgrade`
leaves existing CRDs untouched — this is documented Helm behaviour, not a bug in
the chart — so a chart upgrade that adds a spec field will install an operator
that understands the field while the cluster still rejects it.

The failure is silent. The API server prunes unknown fields rather than
erroring, so `kubectl patch` reports success, the field is dropped on write, and
the operator reads it back as unset:

```console
$ kubectl patch chronikcluster foo --type merge -p '{"spec":{"fixDataOwnership":false}}'
chronikcluster.chronik.io/foo patched
$ kubectl get chronikcluster foo -o jsonpath='{.spec.fixDataOwnership}'
            # empty — silently pruned
```

Apply `crds/` before upgrading, and after upgrading confirm a newly added field
survives a write.

## Rolling a new operator version

The operator decides whether to recreate a broker Pod by comparing a hash of
the CR spec against the Pod's `chronik.io/spec-hash` annotation. It hashes CR
spec fields only, so upgrading the operator normally recreates nothing — but a
change to the *hash function* rebuilds every broker Pod in every cluster it
manages.

To check before rolling out:

```bash
kubectl get chronikcluster <name> -n <ns> -o jsonpath='{.spec}' > spec.json
CHRONIK_SPEC_JSON=spec.json cargo test -p chronik-operator \
    hash_of_a_live_spec -- --ignored --nocapture
```

If the printed hash matches the annotation on the running Pods, the upgrade
will not restart them.

Leadership also does not transfer instantly: the outgoing leader does not
release its Lease on shutdown, so the new one waits for the lease to expire
(~15s) before it starts reconciling.
