//! Optional authentication/authorization hooks.
//!
//! typiql never authenticates anyone itself — same as the C# original, where
//! ASP.NET (JWT bearer, Windows integrated auth) established the user and
//! typiql only read the resulting `HttpContext.User`. Here the app does the
//! same in its own axum middleware, inserting its own user type (anything
//! implementing [`TypiQLUser`]) into the request extensions; the router
//! (see [`crate::TypiQLRouter::with_user`]) copies it into each GraphQL
//! request. An app that never does this (single-user, LAN-only, ...) never
//! pays for any of it: only operations declaring `roles(...)`, or a
//! `scope`/`set` value referencing `@currentUser`, ever look for a user.
//!
//! Login/logout stay app-owned custom resolvers (as C#'s `CustomResolver`s
//! were), setting cookies via `ctx.append_http_header("set-cookie", ...)`.

use std::sync::Arc;

use async_graphql::{Context, Error, Result as GqlResult};
use serde::Serialize;
use serde_json::Value;

/// Implemented by each app's own user type, which carries whatever that app
/// needs (a JWT's claims, AD attributes, a stored user record, ...).
///
/// Every serialized field of the type is readable in declarations as
/// `@currentUser.<field>` — e.g. `scope(owner_sid = "@currentUser.sid")` —
/// so adding a field to the app's user type is all it takes to scope by it.
pub trait TypiQLUser: Serialize + Clone + Send + Sync + 'static {
    /// The value `@currentUser` resolves to (C#: `Identity.Name`).
    fn name(&self) -> &str;

    /// Whether this user satisfies a role named in `roles(...)` (C#:
    /// `IsInRole`). Implemented by the app, so mapping role names onto
    /// whatever the identity provider reports (AD group names, JWT role
    /// claims) happens here — the job C#'s `TypiQLRole` name→value pairs did.
    fn has_role(&self, role: &str) -> bool;
}

/// The request's authenticated user, type-erased so generated code can use
/// it without knowing the app's user type. Custom resolvers wanting the
/// app's concrete type read that directly instead: `ctx.data::<MyUser>()`.
#[derive(Clone)]
pub struct CurrentUser(Arc<dyn ErasedUser>);

trait ErasedUser: Send + Sync {
    fn name(&self) -> &str;
    fn has_role(&self, role: &str) -> bool;
    fn fields(&self) -> &Value;
}

struct Erased<U> {
    user: U,
    fields: Value,
}

impl<U: TypiQLUser> ErasedUser for Erased<U> {
    fn name(&self) -> &str {
        self.user.name()
    }
    fn has_role(&self, role: &str) -> bool {
        self.user.has_role(role)
    }
    fn fields(&self) -> &Value {
        &self.fields
    }
}

impl CurrentUser {
    pub fn new<U: TypiQLUser>(user: U) -> Self {
        let fields = serde_json::to_value(&user).unwrap_or(Value::Null);
        CurrentUser(Arc::new(Erased { user, fields }))
    }

    pub fn name(&self) -> &str {
        self.0.name()
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.0.has_role(role)
    }

    /// A serialized field of the app's user type, by its serialized name.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.0.fields().get(name)
    }
}

/// Attaches `user` to a GraphQL request — both as the app's own type (for
/// custom resolvers) and as [`CurrentUser`] (for generated code). The router
/// does this per request; call it directly when executing a schema without
/// the router (tests, an in-process transport).
pub fn with_user<U: TypiQLUser>(
    request: async_graphql::Request,
    user: U,
) -> async_graphql::Request {
    request.data(CurrentUser::new(user.clone())).data(user)
}

fn current_user<'a>(ctx: &'a Context<'_>) -> GqlResult<&'a CurrentUser> {
    ctx.data_opt::<CurrentUser>()
        .ok_or_else(|| Error::new("Not authenticated"))
}

/// Enforces an operation's `roles(...)`: none declared means open to all
/// (C#: an empty `allowedGroups`); otherwise the user must hold at least one.
pub fn check_roles(ctx: &Context<'_>, roles: &[&str]) -> GqlResult<()> {
    if roles.is_empty() {
        return Ok(());
    }
    let user = current_user(ctx)?;
    if roles.iter().any(|r| user.has_role(r)) {
        Ok(())
    } else {
        Err(Error::new("Access denied"))
    }
}

/// Resolves a declared `scope`/`set` value. Strings starting with `@` are
/// variables — `@currentUser`, `@currentUser.<field>`, `@now` (C#'s
/// `ResolveVariable`); anything else is a literal, returned as-is. A
/// variable that can't be resolved is an error, never a silently dropped
/// filter: an unauthenticated `scope(owner = "@currentUser")` must fail,
/// not widen to every owner.
pub fn resolve_value(ctx: &Context<'_>, value: Value) -> GqlResult<Value> {
    let Value::String(s) = &value else {
        return Ok(value);
    };
    let Some(var) = s.strip_prefix('@') else {
        return Ok(value);
    };
    match var.split_once('.') {
        None if var == "currentUser" => Ok(Value::String(current_user(ctx)?.name().to_string())),
        None if var == "now" => Ok(Value::String(now_rfc3339())),
        Some(("currentUser", field)) => current_user(ctx)?
            .field(field)
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| Error::new(format!("Current user has no `{field}`"))),
        _ => Err(Error::new(format!("Unknown variable `{s}`"))),
    }
}

/// UTC now as RFC 3339 (`2026-09-24T13:05:09Z`), without a date-time crate.
pub(crate) fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339(secs)
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn formats_utc_seconds() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_790_251_509), "2026-09-24T12:05:09Z");
    }
}
