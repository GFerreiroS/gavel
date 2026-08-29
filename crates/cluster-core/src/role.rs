//! Roles.
//!
//! A node has a *set* of roles, never a single role. The set is a bitmask so
//! it stays compact on the wire and needs no heap allocation.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Gateway,
    Frontend,
    Backend,
    Compute,
    Storage,
    Coordinator,
}

pub const ALL_ROLES: [Role; 6] = [
    Role::Gateway,
    Role::Frontend,
    Role::Backend,
    Role::Compute,
    Role::Storage,
    Role::Coordinator,
];

/// Which roles survive when the cluster is degraded and cannot satisfy every
/// minimum. Expressed as data so the policy can evolve without
/// touching scheduling code.
pub const DEGRADATION_PRIORITY: [Role; 6] = [
    Role::Gateway,
    Role::Frontend,
    Role::Backend,
    Role::Storage,
    Role::Coordinator,
    Role::Compute,
];

impl Role {
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn bit(self) -> u8 {
        1 << (self as u8)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Gateway => "gateway",
            Role::Frontend => "frontend",
            Role::Backend => "backend",
            Role::Compute => "compute",
            Role::Storage => "storage",
            Role::Coordinator => "coordinator",
        }
    }

    /// Position in [`DEGRADATION_PRIORITY`]; lower means "give this up last".
    pub fn shed_priority(self) -> usize {
        DEGRADATION_PRIORITY
            .iter()
            .position(|r| *r == self)
            .unwrap_or(usize::MAX)
    }

    pub fn parse(s: &str) -> Option<Role> {
        ALL_ROLES.into_iter().find(|r| r.as_str() == s)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of roles held by one node, packed into a single byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleSet(pub u8);

impl RoleSet {
    pub const EMPTY: RoleSet = RoleSet(0);

    pub fn from_roles(roles: impl IntoIterator<Item = Role>) -> Self {
        let mut set = RoleSet::EMPTY;
        for role in roles {
            set.insert(role);
        }
        set
    }

    pub const fn contains(self, role: Role) -> bool {
        self.0 & role.bit() != 0
    }

    pub fn insert(&mut self, role: Role) -> bool {
        let had = self.contains(role);
        self.0 |= role.bit();
        !had
    }

    pub fn remove(&mut self, role: Role) -> bool {
        let had = self.contains(role);
        self.0 &= !role.bit();
        had
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn iter(self) -> impl Iterator<Item = Role> {
        ALL_ROLES.into_iter().filter(move |r| self.contains(*r))
    }
}

impl fmt::Display for RoleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for role in self.iter() {
            if !first {
                f.write_str(", ")?;
            }
            f.write_str(role.as_str())?;
            first = false;
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// Desired replica count for one role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePolicy {
    pub min_replicas: usize,
    pub max_replicas: Option<usize>,
}

impl RolePolicy {
    pub const fn new(min_replicas: usize) -> Self {
        Self {
            min_replicas,
            max_replicas: None,
        }
    }
}

impl Default for RolePolicy {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Policies for every role, indexed by [`Role::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePolicies([RolePolicy; 6]);

impl RolePolicies {
    pub fn new(policies: [RolePolicy; 6]) -> Self {
        Self(policies)
    }

    pub fn get(&self, role: Role) -> RolePolicy {
        self.0[role.index()]
    }

    pub fn set(&mut self, role: Role, policy: RolePolicy) {
        self.0[role.index()] = policy;
    }

    /// Roles whose minimum is not met, in degradation-priority order: the
    /// first entry is the one to fix first. This is the hook a future
    /// autoscaler / role-rebalancer plugs into.
    pub fn unmet(&self, counts: impl Fn(Role) -> usize) -> Vec<(Role, usize)> {
        let mut deficits: Vec<(Role, usize)> = ALL_ROLES
            .into_iter()
            .filter_map(|role| {
                let want = self.get(role).min_replicas;
                let have = counts(role);
                // `then`, not `then_some`: the latter would evaluate the
                // subtraction even when `have >= want`.
                (have < want).then(|| (role, want - have))
            })
            .collect();
        deficits.sort_by_key(|(role, _)| role.shed_priority());
        deficits
    }
}

impl Default for RolePolicies {
    /// Default role replica policies.
    fn default() -> Self {
        let mut p = RolePolicies([RolePolicy::default(); 6]);
        p.set(Role::Gateway, RolePolicy::new(1));
        p.set(Role::Frontend, RolePolicy::new(2));
        p.set(Role::Backend, RolePolicy::new(2));
        p.set(Role::Storage, RolePolicy::new(1));
        p.set(Role::Coordinator, RolePolicy::new(1));
        p
    }
}
