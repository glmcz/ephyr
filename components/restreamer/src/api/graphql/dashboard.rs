//! Dashboard [GraphQL] API providing application usage.
//!
//! [GraphQL]: https://graphql.com

use juniper::{graphql_object, RootNode, EmptyMutation, EmptySubscription};
use super::Context;

/// Schema of `Dashboard` app.
pub type Schema =
RootNode<'static, QueriesRoot, EmptyMutation<Context>, EmptySubscription<Context>>;

/// Constructs and returns new [`Schema`], ready for use.
#[inline]
#[must_use]
pub fn schema() -> Schema {
    Schema::new(QueriesRoot, EmptyMutation::new(), EmptySubscription::new())
}

/// Root of all [GraphQL queries][1] in the [`Schema`].
///
/// [1]: https://spec.graphql.org/June2018/#sec-Root-Operation-Types
#[derive(Clone, Copy, Debug)]
pub struct QueriesRoot;

#[graphql_object(name = "Query", context = Context)]
impl QueriesRoot {
    fn statistics(context: &Context) -> Option<String> {
        context.config().public_host.clone()
    }
}
