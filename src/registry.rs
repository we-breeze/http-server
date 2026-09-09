use std::fmt;

use crate::router::RouteDescriptor;
use crate::{Authenticator, Router};

/// The typed registration emitted by function API macros for a handler collection.
#[doc(hidden)]
pub struct ApiRegistration<S, A: Authenticator> {
    pub name: &'static str,
    pub routes: fn() -> &'static [RouteDescriptor],
    pub build: fn(&S) -> Router<A>,
}

/// Two automatically collected routes would depend on registration order.
///
/// The collection is rejected before any API constructors are invoked. Routes
/// with different HTTP methods or different matching priorities may coexist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryError {
    pub first_api: &'static str,
    pub first_path: &'static str,
    pub second_api: &'static str,
    pub second_path: &'static str,
    /// HTTP method bits shared by the conflicting routes.
    pub methods: u16,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("conflicting ")?;
        let mut separator = "";
        for (index, method) in ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"]
            .into_iter()
            .enumerate()
        {
            if self.methods & (1 << index) != 0 {
                write!(formatter, "{separator}{method}")?;
                separator = ", ";
            }
        }
        write!(
            formatter,
            " routes (method bits {:#06x}): {} `{}` and {} `{}`",
            self.methods, self.first_api, self.first_path, self.second_api, self.second_path,
        )
    }
}

impl std::error::Error for RegistryError {}

/// Validates a collection and constructs its flat router in API-name order.
#[doc(hidden)]
pub fn collect<S, A: Authenticator>(
    state: &S,
    registrations: &[ApiRegistration<S, A>],
) -> Result<Router<A>, RegistryError> {
    let mut ordered: Vec<_> = registrations.iter().collect();
    ordered.sort_by_key(|registration| registration.name);

    let mut routes: Vec<(&str, &RouteDescriptor)> = Vec::new();
    for registration in &ordered {
        for route in (registration.routes)() {
            for &(other_api, other) in &routes {
                let methods = route.methods & other.methods;
                if methods != 0
                    && (equivalent_shape(route.path, other.path)
                        || (route.priority == other.priority
                            && patterns_overlap(route.path, other.path)))
                {
                    return Err(RegistryError {
                        first_api: other_api,
                        first_path: other.path,
                        second_api: registration.name,
                        second_path: route.path,
                        methods,
                    });
                }
            }
            routes.push((registration.name, route));
        }
    }

    Ok(ordered.into_iter().fold(Router::default(), |router, api| {
        router.merge((api.build)(state))
    }))
}

fn equivalent_shape(left: &str, right: &str) -> bool {
    let mut left = left.split('/');
    let mut right = right.split('/');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(left), Some(right))
                if left == right
                    || (left.starts_with(':') && right.starts_with(':'))
                    || (left.starts_with('*') && right.starts_with('*')) => {}
            _ => return false,
        }
    }
}

// Templates have already been validated by route macros: parameters consume one
// nonempty segment, and a terminal catch-all accepts any remaining suffix,
// including an absent suffix. Preserve empty literal segments to mirror routing.
fn patterns_overlap(left: &str, right: &str) -> bool {
    let mut left = left.split('/');
    let mut right = right.split('/');
    loop {
        match (left.next(), right.next()) {
            (Some(segment), _) if segment.starts_with('*') => return true,
            (_, Some(segment)) if segment.starts_with('*') => return true,
            (None, None) => return true,
            (Some(left), Some(right)) => {
                let left_parameter = left.starts_with(':');
                let right_parameter = right.starts_with(':');
                if left_parameter && right_parameter {
                    continue;
                }
                if left_parameter {
                    if right.is_empty() {
                        return false;
                    }
                } else if right_parameter {
                    if left.is_empty() {
                        return false;
                    }
                } else if left != right {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{ApiMetrics, NoAuthenticator};

    type State = Mutex<Vec<&'static str>>;

    fn metrics() -> ApiMetrics {
        panic!("collect must not initialize request metrics")
    }

    const fn route(path: &'static str, methods: u16, priority: usize) -> RouteDescriptor {
        RouteDescriptor {
            path,
            methods,
            priority,
            metrics,
        }
    }

    macro_rules! routes {
        ($($route:expr),* $(,)?) => {
            (|| const { &[$($route),*] }) as fn() -> &'static [RouteDescriptor]
        };
    }

    fn alpha(state: &State) -> Router {
        state.lock().unwrap().push("Alpha");
        Router::default()
    }

    fn beta(state: &State) -> Router {
        state.lock().unwrap().push("Beta");
        Router::default()
    }

    fn registrations(
        left: fn() -> &'static [RouteDescriptor],
        right: fn() -> &'static [RouteDescriptor],
    ) -> [ApiRegistration<State, NoAuthenticator>; 2] {
        [
            ApiRegistration {
                name: "Beta",
                routes: right,
                build: beta,
            },
            ApiRegistration {
                name: "Alpha",
                routes: left,
                build: alpha,
            },
        ]
    }

    #[test]
    fn validates_before_construction_and_reports_shared_methods() {
        let state = State::default();
        let registrations = registrations(
            routes!(route("/items/:item", 2 | 32, 10)),
            routes!(route("/items/:other", 2, 10)),
        );
        let error = collect(&state, &registrations).err().unwrap();
        assert_eq!(error.first_api, "Alpha");
        assert_eq!(error.second_api, "Beta");
        assert_eq!(error.first_path, "/items/:item");
        assert_eq!(error.second_path, "/items/:other");
        assert_eq!(error.methods, 2);
        let display = error.to_string();
        assert!(display.contains("GET"));
        assert!(!display.contains("POST"));
        assert!(display.contains("Alpha `/items/:item`"));
        assert!(display.contains("Beta `/items/:other`"));
        assert!(state.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_ambiguous_equal_priority_routes_including_same_api() {
        for (left, right) in [
            (
                routes!(route("/items/:id/view", 2, 10)),
                routes!(route("/items/new/:action", 2, 10)),
            ),
            (
                routes!(route("/items/:id/*rest", 2, 10)),
                routes!(route("/items/new/*tail", 2, 10)),
            ),
            (
                routes!(route("/items/:id", 2, 10), route("/items/:other", 2, 10)),
                routes!(),
            ),
        ] {
            let state = State::default();
            assert!(collect(&state, &registrations(left, right)).is_err());
            assert!(state.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn permits_disjoint_methods_and_specificity_then_constructs_once_in_name_order() {
        let state = State::default();
        let registrations = registrations(
            routes!(
                route("/items/:id", 2, 10),
                route("/items/new", 2, 20),
                route("/items/*tail", 2, 0),
                route("/items/:id/view", 2, 30),
            ),
            routes!(
                route("/items/:other", 32, 10),
                route("/items/:id/edit", 2, 30),
            ),
        );
        assert!(collect(&state, &registrations).is_ok());
        assert_eq!(*state.lock().unwrap(), ["Alpha", "Beta"]);
        assert!(collect::<_, NoAuthenticator>(&State::default(), &[]).is_ok());
    }

    #[test]
    fn equivalent_shapes_remain_conflicts_with_different_priorities() {
        let state = State::default();
        let registrations = registrations(
            routes!(route("/items/:item", 2, 10)),
            routes!(route("/items/:other", 2, 20)),
        );
        assert!(collect(&state, &registrations).is_err());
        assert!(state.lock().unwrap().is_empty());
    }

    #[test]
    fn overlap_respects_empty_segments_and_terminal_catch_all() {
        for (left, right, expected) in [
            ("/items/:id", "/items/new", true),
            ("/items/:id", "/items/", false),
            ("/items/:id", "/items", false),
            ("/items/:id", "/items/new/view", false),
            ("/items/:id/view", "/items/new/:action", true),
            ("/items/:id/view", "/items/:other/edit", false),
            ("/items/*rest", "/items", true),
            ("/items/*rest", "/items/", true),
            ("/items/*rest", "/items/:id/view", true),
            ("/items/:id/*rest", "/items/", false),
            ("/items/:id/*rest", "/items/new", true),
            ("/items/:id/*rest", "/items//view", false),
            ("/items/:id/*rest", "/other/*rest", false),
            ("/items/*rest", "/items/:id/*tail", true),
            ("/items//:id", "/items/:other/view", false),
            ("/items/", "/items", false),
            ("/", "/*rest", true),
        ] {
            assert_eq!(patterns_overlap(left, right), expected, "{left}, {right}");
            assert_eq!(patterns_overlap(right, left), expected, "{right}, {left}");
        }
    }
}
