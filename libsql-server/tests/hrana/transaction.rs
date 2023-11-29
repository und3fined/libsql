use crate::common::net::TurmoilConnector;
use libsql::{params, Database, TransactionBehavior};

#[test]
fn transaction_commit_and_rollback() {
    let mut sim = turmoil::Builder::new().build();
    sim.host("primary", super::make_standalone_server);
    sim.client("client", async {
        let db = Database::open_remote_with_connector("http://primary:8080", "", TurmoilConnector)?;
        let conn = db.connect()?;

        // initialize tables
        let tx = conn.transaction().await?;
        tx.execute_batch(r#"create table t(x text);"#).await?;
        tx.commit().await?;

        // transaction with temporary data
        let tx = conn.transaction().await?;
        tx.execute("insert into t(x) values('hello');", ()).await?;

        let mut rows = tx
            .query("select * from t where x = ?", params!["hello"])
            .await?;

        assert_eq!(rows.column_count(), 1);
        assert_eq!(rows.column_name(0), Some("x"));
        assert_eq!(rows.next()?.unwrap().get::<String>(0)?, "hello");
        assert!(rows.next()?.is_none());
        tx.rollback().await?;

        // confirm that temporary that was not committed
        let mut rows = conn
            .query("select * from t where x = ?", params!["hello"])
            .await?;

        assert_eq!(rows.column_count(), 1);
        assert_eq!(rows.column_name(0), Some("x"));
        assert!(rows.next()?.is_none());

        Ok(())
    });

    sim.run().unwrap();
}

#[test]
fn multiple_concurrent_transactions() {
    let mut sim = turmoil::Builder::new().build();
    sim.host("primary", super::make_standalone_server);
    sim.client("client", async {
        let db = Database::open_remote_with_connector("http://primary:8080", "", TurmoilConnector)?;
        let conn = db.connect()?;
        conn.execute_batch(r#"create table t(x text);"#).await?;

        // open first transaction and alter data
        let tx1 = conn
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .await?;
        tx1.execute("insert into t(x) values('hello');", ()).await?;

        // while first transaction is still open open another read-only transaction and try to read
        let tx2 = conn
            .transaction_with_behavior(TransactionBehavior::ReadOnly)
            .await?;
        let mut rows = tx2
            .query("select * from t where x = ?", params!["hello"])
            .await?;
        assert_eq!(rows.column_count(), 1);
        assert_eq!(rows.column_name(0), Some("x"));
        assert!(rows.next()?.is_none());

        // commit first transaction - T2 should still read old data
        tx1.commit().await?;

        let mut rows = tx2
            .query("select * from t where x = ?", params!["hello"])
            .await?;
        assert_eq!(rows.column_count(), 1);
        assert_eq!(rows.column_name(0), Some("x"));
        assert!(rows.next()?.is_none());
        tx2.commit().await?;

        // finally open new transaction - it now should read actual data
        let tx3 = conn
            .transaction_with_behavior(TransactionBehavior::ReadOnly)
            .await?;
        let mut rows = tx3
            .query("select * from t where x = ?", params!["hello"])
            .await?;
        assert_eq!(rows.column_count(), 1);
        assert_eq!(rows.column_name(0), Some("x"));
        assert_eq!(rows.next()?.unwrap().get::<String>(0)?, "hello");
        assert!(rows.next()?.is_none());

        Ok(())
    });
    sim.run().unwrap();
}
