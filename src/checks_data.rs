use crate::util::PermissionRequirement;

pub struct ApiRequirement {
    pub api_group: &'static str,
    pub resource: &'static str,
    pub label: &'static str,
}

pub const REQUIRED_API_RESOURCES: &[ApiRequirement] = &[
    ApiRequirement {
        api_group: "apps",
        resource: "deployments",
        label: "Deployments",
    },
    ApiRequirement {
        api_group: "",
        resource: "services",
        label: "Services",
    },
    ApiRequirement {
        api_group: "",
        resource: "serviceaccounts",
        label: "ServiceAccounts",
    },
    ApiRequirement {
        api_group: "",
        resource: "secrets",
        label: "Secrets",
    },
    ApiRequirement {
        api_group: "",
        resource: "configmaps",
        label: "ConfigMaps",
    },
    ApiRequirement {
        api_group: "",
        resource: "persistentvolumeclaims",
        label: "PersistentVolumeClaims",
    },
    ApiRequirement {
        api_group: "networking.k8s.io",
        resource: "networkpolicies",
        label: "NetworkPolicies",
    },
    ApiRequirement {
        api_group: "rbac.authorization.k8s.io",
        resource: "clusterroles",
        label: "ClusterRoles",
    },
    ApiRequirement {
        api_group: "rbac.authorization.k8s.io",
        resource: "clusterrolebindings",
        label: "ClusterRoleBindings",
    },
];

pub const REQUIRED_PERMISSIONS: &[PermissionRequirement] = &[
    PermissionRequirement {
        verb: "create",
        resource: "namespaces",
        namespace: None,
    },
    PermissionRequirement {
        verb: "create",
        resource: "deployments.apps",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "services",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "serviceaccounts",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "secrets",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "configmaps",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "persistentvolumeclaims",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "networkpolicies.networking.k8s.io",
        namespace: Some("default"),
    },
    PermissionRequirement {
        verb: "create",
        resource: "clusterroles.rbac.authorization.k8s.io",
        namespace: None,
    },
    PermissionRequirement {
        verb: "create",
        resource: "clusterrolebindings.rbac.authorization.k8s.io",
        namespace: None,
    },
];
