//! MatrixOne snapshot SQL helpers.
//!
//! Snapshots should target the specific database, not the entire account/cluster.
//! Identifiers are backtick-quoted, and embedded backticks are escaped to prevent
//! SQL injection.
//! Syntax: ``CREATE SNAPSHOT `{name}` FOR DATABASE `{db}` ``

/// Validate a SQL identifier: non-empty, alphanumeric + underscore only.
/// Rejects backticks, quotes, spaces, and other special characters.
pub fn validate_sql_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("empty {label}"));
    }
    if value.len() > 64 {
        return Err(format!(
            "invalid {label} '{value}': MySQL/MatrixOne identifiers are at most 64 ASCII bytes"
        ));
    }
    if !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "invalid {label} '{value}': only [a-zA-Z0-9_] allowed"
        ));
    }
    Ok(())
}

/// Backtick-quote a MySQL/MatrixOne identifier (`a`b` → ``a``b``).
pub(crate) fn quote_mysql_identifier(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

/// ``CREATE SNAPSHOT `{name}` FOR DATABASE `{db}` ``.
///
/// All identifiers are backtick-quoted, with embedded backticks escaped.
pub fn create_snapshot_for_db_sql(name: &str, db: &str) -> String {
    format!(
        "CREATE SNAPSHOT {} FOR DATABASE {}",
        quote_mysql_identifier(name),
        quote_mysql_identifier(db)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_snapshot_for_database() {
        assert_eq!(
            create_snapshot_for_db_sql("sp1", "astra_runtime"),
            "CREATE SNAPSHOT `sp1` FOR DATABASE `astra_runtime`"
        );
    }

    #[test]
    fn create_snapshot_escapes_backticks() {
        assert_eq!(
            create_snapshot_for_db_sql("sp`1", "astra`runtime"),
            "CREATE SNAPSHOT `sp``1` FOR DATABASE `astra``runtime`"
        );
    }

    #[test]
    fn validate_sql_identifier_accepts_valid() {
        assert!(validate_sql_identifier("task_123", "name").is_ok());
        assert!(validate_sql_identifier("astra_runtime", "db").is_ok());
        assert!(validate_sql_identifier("sys", "account").is_ok());
    }

    #[test]
    fn validate_sql_identifier_rejects_injection() {
        assert!(validate_sql_identifier("", "name").is_err());
        assert!(validate_sql_identifier("x'; DROP--", "name").is_err());
        assert!(validate_sql_identifier("has spaces", "name").is_err());
        assert!(validate_sql_identifier("back`tick", "name").is_err());
        assert!(validate_sql_identifier("path/sep", "name").is_err());
        assert!(validate_sql_identifier(&"a".repeat(65), "database").is_err());
    }
}
