//! The LDAP / Active Directory adapter against a real directory — needs one:
//! `TYPIQL_TEST_LDAP="ldap://127.0.0.1:389|dc=example,dc=org|cn=admin,dc=example,dc=org|password"`
//! (URL, base DN, bind DN, password). Each test works in its own OU.
//!
//! Directory entries don't fit the conformance suite's types (nested data,
//! floats), so this checks the adapter's own contract: typed attributes,
//! multi-valued ones, the directory's matching rules, sorting and paging,
//! relations through `dn`, and writes.

use std::collections::HashMap;
use std::sync::Arc;

use ldap3::{LdapConnAsync, Scope, SearchEntry};
use serde_json::{json, Value};
use typiql::{AdapterMap, TypiQLAdapter};
use typiql_adapter_ldap::{Bind, LdapAdapter, LdapClass};

#[typiql::typiql_type(adapter = "ldap")]
pub struct Account {
    /// The entry's name: clients choose it.
    #[typiql(key, input, data_name = "uid")]
    pub username: String,
    #[typiql(data_name = "cn")]
    pub name: String,
    #[typiql(data_name = "sn")]
    pub surname: String,
    pub mail: Vec<String>,
    #[typiql(data_name = "uidNumber")]
    pub uid_number: i64,
    #[typiql(data_name = "gidNumber")]
    pub gid_number: i64,
    #[typiql(data_name = "homeDirectory")]
    pub home: String,
    pub description: Option<String>,
    #[typiql(data_name = "jpegPhoto")]
    pub photo: Option<String>,
    pub dn: Option<String>,
    /// Groups listing this account's DN as a member.
    #[typiql(relation(local = "dn", foreign = "members"))]
    pub groups: Vec<Group>,
}

#[typiql::typiql_type(adapter = "ldap")]
pub struct Group {
    #[typiql(key, input, data_name = "cn")]
    pub name: String,
    #[typiql(data_name = "member")]
    pub members: Vec<String>,
    pub description: Option<String>,
}

mod schema {
    use super::*;
    typiql::typiql_schema!(Account, Group);
}

struct Directory {
    url: String,
    base: String,
    bind_dn: String,
    password: String,
}

impl Directory {
    fn from_env() -> Option<Self> {
        let v = std::env::var("TYPIQL_TEST_LDAP").ok()?;
        let parts: Vec<&str> = v.splitn(4, '|').collect();
        let [url, base, bind_dn, password] = parts[..] else {
            panic!("TYPIQL_TEST_LDAP is url|base|bind dn|password");
        };
        Some(Directory { url: url.into(), base: base.into(), bind_dn: bind_dn.into(), password: password.into() })
    }

    async fn conn(&self) -> ldap3::Ldap {
        let (c, mut ldap) = LdapConnAsync::new(&self.url).await.unwrap();
        ldap3::drive!(c);
        ldap.simple_bind(&self.bind_dn, &self.password).await.unwrap().success().unwrap();
        ldap
    }

    async fn add_ou(&self, dn: &str, name: &str) {
        let mut ldap = self.conn().await;
        let attrs = vec![("objectClass", ["organizationalUnit"].into()), ("ou", [name].into())];
        ldap.add(dn, attrs).await.unwrap().success().unwrap();
    }

    /// Deletes `dn` and everything under it, deepest first.
    async fn remove_tree(&self, dn: &str) {
        let mut ldap = self.conn().await;
        let (entries, _) = ldap.search(dn, Scope::Subtree, "(objectClass=*)", vec!["1.1"]).await.unwrap().success().unwrap();
        let mut dns: Vec<String> = entries.into_iter().map(|e| SearchEntry::construct(e).dn).collect();
        dns.sort_by_key(|d| std::cmp::Reverse(d.len()));
        for d in dns {
            ldap.delete(&d).await.unwrap();
        }
    }
}

#[tokio::test]
async fn directory_entries_as_types() {
    let Some(dir) = Directory::from_env() else {
        eprintln!("skipped: set TYPIQL_TEST_LDAP to run against a directory");
        return;
    };
    let ou = format!("t{}", uuid::Uuid::new_v4().simple());
    let root = format!("ou={ou},{}", dir.base);
    dir.add_ou(&root, &ou).await;
    dir.add_ou(&format!("ou=people,{root}"), "people").await;
    dir.add_ou(&format!("ou=groups,{root}"), "groups").await;

    let adapter = LdapAdapter::new(&dir.url, &dir.base, Bind::Simple { dn: dir.bind_dn.clone(), password: dir.password.clone() })
        .class(
            "accounts",
            LdapClass::new("(objectClass=inetOrgPerson)")
                .base(&format!("ou=people,{root}"))
                .creatable("uid", &["inetOrgPerson", "posixAccount"]),
        )
        .class(
            "groups",
            LdapClass::new("(objectClass=groupOfNames)").base(&format!("ou=groups,{root}")).creatable("cn", &["groupOfNames"]),
        );
    let adapter: Arc<dyn TypiQLAdapter> = Arc::new(adapter);
    let mut adapters: AdapterMap = HashMap::new();
    adapters.insert("ldap", adapter.clone());
    let schema = schema::build_typiql_schema(adapters);
    let try_run = |q: String| {
        let schema = schema.clone();
        async move {
            let res = schema.execute(q.as_str()).await;
            if res.errors.is_empty() {
                Ok(res.data.into_json().unwrap())
            } else {
                Err(format!("{:?}", res.errors))
            }
        }
    };
    let run = |q: String| {
        let r = try_run(q.clone());
        async move { r.await.unwrap_or_else(|e| panic!("{q}\n  -> {e}")) }
    };
    let names = |v: &Value| -> Vec<String> {
        v.as_array().unwrap().iter().map(|r| r["username"].as_str().unwrap().to_string()).collect()
    };

    // Adding: typed and multi-valued attributes, the new entry's DN.
    let mut dns = HashMap::new();
    for (user, name, number, mail, description) in [
        ("alice", "Alice Smith", 1001, r#"["alice@example.org", "a.smith@example.org"]"#, "null"),
        ("bob", "Bob Stone", 1002, "[]", r#""Builder""#),
        ("carol", "Carol Smithers", 1003, r#"["carol@example.org"]"#, "null"),
    ] {
        let added = run(format!(
            r#"mutation {{ addAccount(values: {{ username: "{user}", name: "{name}", surname: "{}", mail: {mail}, uidNumber: {number}, gidNumber: 100, home: "/home/{user}", description: {description} }}) {{ username dn uidNumber mail }} }}"#,
            name.split(' ').nth(1).unwrap()
        ))
        .await;
        assert_eq!(added["addAccount"]["uidNumber"], number);
        dns.insert(user, added["addAccount"]["dn"].as_str().unwrap().to_string());
    }
    assert_eq!(dns["alice"], format!("uid=alice,ou=people,{root}"));
    run(format!(
        r#"mutation {{ addGroup(values: {{ name: "admins", members: ["{}", "{}"] }}) {{ name }} }}"#,
        dns["alice"], dns["bob"]
    ))
    .await;

    // The directory's matching rules: uid and cn compare regardless of case.
    // (Attribute values are a set: a list's order isn't kept.)
    let mut alice = run(r#"{ getAccount(username: "ALICE") { username name mail description photo } }"#.into()).await;
    alice["getAccount"]["mail"].as_array_mut().unwrap().sort_by_key(|v| v.as_str().unwrap().to_string());
    assert_eq!(
        alice["getAccount"],
        json!({ "username": "alice", "name": "Alice Smith", "mail": ["a.smith@example.org", "alice@example.org"], "description": null, "photo": null })
    );
    let q = |args: &str| format!("{{ getAccounts({args}, _orderBy: \"username\") {{ username }} }}");
    for (args, expected) in [
        (r#"name_contains: "SMITH""#, vec!["alice", "carol"]),
        (r#"name_notContains: "smith""#, vec!["bob"]),
        (r#"name_startsWith: "b""#, vec!["bob"]),
        (r#"name_endsWith: "ers""#, vec!["carol"]),
        (r#"username_in: ["BOB", "carol"]"#, vec!["bob", "carol"]),
        (r#"username_notIn: ["bob"]"#, vec!["alice", "carol"]),
        (r#"username_not: "alice""#, vec!["bob", "carol"]),
        ("uidNumber_gt: 1001", vec!["bob", "carol"]),
        ("uidNumber_gte: 1002", vec!["bob", "carol"]),
        ("uidNumber_lt: 1002", vec!["alice"]),
        ("uidNumber_lte: 1002", vec!["alice", "bob"]),
        (r#"mail_anyEq: ["a.smith@example.org", "nobody@example.org"]"#, vec!["alice"]),
        (r#"mail_anyNe: ["alice@example.org"]"#, vec!["bob", "carol"]),
        (r#"description: "builder""#, vec!["bob"]),
        (r#"name_contains: "*""#, vec![]),
        // Text ordering isn't LDAP's: done in memory, by code point.
        (r#"name_gt: "Bob""#, vec!["bob", "carol"]),
    ] {
        let got = run(q(args)).await;
        assert_eq!(names(&got["getAccounts"]), expected, "{args}");
    }

    // Sorting, paging, PageInfo.
    let sorted = run(r#"{ getAccounts(_orderBy_desc: "uidNumber") { username } }"#.into()).await;
    assert_eq!(names(&sorted["getAccounts"]), ["carol", "bob", "alice"]);
    let page = run(r#"{ getAccounts(_orderBy: "name", _start: 1, _limit: 1) { username } }"#.into()).await;
    assert_eq!(names(&page["getAccounts"]), ["bob"]);
    let info = run(r#"{ getAccountsPageInfo(_limit: 2) { totalResults totalPages } }"#.into()).await;
    assert_eq!(info["getAccountsPageInfo"], json!({ "totalResults": 3, "totalPages": 2 }));

    // Relations through DNs, both ways.
    let groups = run(r#"{ getAccounts(_orderBy: "username") { username groups { name } } }"#.into()).await;
    assert_eq!(
        groups["getAccounts"],
        json!([
            { "username": "alice", "groups": [{ "name": "admins" }] },
            { "username": "bob", "groups": [{ "name": "admins" }] },
            { "username": "carol", "groups": [] },
        ])
    );
    let members = run(r#"{ getGroup(name: "Admins") { name members } }"#.into()).await;
    assert_eq!(members["getGroup"]["members"].as_array().unwrap().len(), 2);

    // Updating: a binary attribute as base64, and "" removing a value.
    let photo = "/9j/4AAQSkZJRgABAQ==";
    let updated = run(format!(
        r#"mutation {{ updateAccount(username: "bob", update: {{ description: "", photo: "{photo}", mail: ["bob@example.org"] }}) {{ description photo mail }} }}"#
    ))
    .await;
    assert_eq!(updated["updateAccount"], json!({ "description": null, "photo": photo, "mail": ["bob@example.org"] }));

    // Keys are unique; a batch is all or nothing.
    let dup = try_run(
        r#"mutation { addAccount(values: { username: "Alice", name: "A", surname: "A", mail: [], uidNumber: 9, gidNumber: 9, home: "/" }) { username } }"#.into(),
    )
    .await
    .unwrap_err();
    assert!(dup.contains("Duplicate key"), "{dup}");
    let row = |u: &str| json!({ "uid": u, "cn": u, "sn": u, "uidNumber": 5, "gidNumber": 5, "homeDirectory": "/" });
    let err = adapter.add_many("accounts".into(), "uid", vec![row("dave"), row("alice")]).await.unwrap_err();
    assert!(err.starts_with("Duplicate key"), "{err}");
    assert_eq!(run(r#"{ getAccount(username: "dave") { username } }"#.into()).await["getAccount"], Value::Null);

    // Removing.
    let removed = run(r#"mutation { removeAccount(username: "carol") { username } }"#.into()).await;
    assert_eq!(removed["removeAccount"]["username"], "carol");
    let left = run(r#"{ getAccounts(_orderBy: "username") { username } }"#.into()).await;
    assert_eq!(names(&left["getAccounts"]), ["alice", "bob"]);

    drop(schema);
    dir.remove_tree(&root).await;
}
