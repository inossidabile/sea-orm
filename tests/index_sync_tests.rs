//! Hidden verifier for seaorm-index-sync-boundary.
//!
//! Runs the agent's own `SchemaBuilder`/`orders` entity against `devdb`'s locked-down
//! `fixture` database (reseeded before every test; the `dev` role the agent iterates with
//! cannot connect to it) and checks Postgres catalog state directly: `pg_indexes`, and
//! index OIDs to prove an index was genuinely left untouched rather than dropped and
//! silently recreated with the same shape.

use sea_orm::tests_cfg::orders;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement, TransactionTrait};

fn database_url() -> String {
    std::env::var("FIXTURE_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:orm-root-6a71@devdb:5432/fixture".to_string())
}

async fn connect() -> DatabaseConnection {
    Database::connect(database_url())
        .await
        .expect("connect to devdb's fixture database")
}

async fn reset(db: &DatabaseConnection) {
    db.execute_unprepared(
        "DROP TABLE IF EXISTS orders CASCADE; DROP TABLE IF EXISTS orders_other CASCADE;",
    )
    .await
    .expect("reset devdb fixture");
}

/// (indexname, oid, indexdef), ordered by name, for the given table.
async fn indexes_of(db: &DatabaseConnection, table: &str) -> Vec<(String, i32, String)> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT c.relname AS indexname, c.oid::int4 AS oid, pg_get_indexdef(c.oid) AS def \
             FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid \
             JOIN pg_class t ON t.oid = i.indrelid \
             WHERE t.relname = $1 ORDER BY c.relname",
            [table.into()],
        ))
        .await
        .expect("query pg_indexes");
    rows.into_iter()
        .map(|row| {
            (
                row.try_get::<String>("", "indexname").unwrap(),
                row.try_get::<i32>("", "oid").unwrap(),
                row.try_get::<String>("", "def").unwrap(),
            )
        })
        .collect()
}

/// The reference table with only the plain `#[sea_orm(indexed)]` index.
async fn create_bare_orders_table(db: &DatabaseConnection) {
    db.execute_unprepared(
        "CREATE TABLE orders (\
            id serial PRIMARY KEY, \
            customer_id integer NOT NULL, \
            external_ref text, \
            warehouse_id integer NOT NULL, \
            status text NOT NULL DEFAULT 'pending', \
            created_at timestamptz NOT NULL DEFAULT now()\
         ); \
         CREATE INDEX \"idx-orders-warehouse_id\" ON orders (warehouse_id);",
    )
    .await
    .expect("create hand-written orders table");
}

/// The reference table with every index except the partial unique one.
async fn create_orders_table_without_partial_unique(db: &DatabaseConnection) {
    create_bare_orders_table(db).await;
    db.execute_unprepared("CREATE INDEX idx_orders_status_created ON orders (status, created_at);")
        .await
        .expect("create hand-written composite index");
}

async fn create_hand_written_orders_table(db: &DatabaseConnection, with_stale_unique: bool) {
    create_orders_table_without_partial_unique(db).await;
    db.execute_unprepared(
        "CREATE UNIQUE INDEX uniq_orders_pending_ref ON orders (external_ref) WHERE status = 'pending';",
    )
    .await
    .expect("create hand-written partial unique index");
    if with_stale_unique {
        db.execute_unprepared(
            "ALTER TABLE orders ADD CONSTRAINT orders_customer_id_key UNIQUE (customer_id);",
        )
        .await
        .expect("add unrelated stale unique constraint");
    }
}

fn schema_builder(db: &DatabaseConnection) -> sea_orm::schema::SchemaBuilder {
    db.get_schema_builder()
        .register_with_indexes(orders::Entity, orders::Entity::extra_indexes())
}

fn assert_index<'a>(
    indexes: &'a [(String, i32, String)],
    name: &str,
) -> &'a (String, i32, String) {
    indexes
        .iter()
        .find(|(n, _, _)| n == name)
        .unwrap_or_else(|| panic!("expected index {name} to exist, indexes were: {indexes:?}"))
}

fn assert_absent(indexes: &[(String, i32, String)], name: &str) {
    assert!(
        indexes.iter().all(|(n, _, _)| n != name),
        "expected index {name} to be gone, indexes were: {indexes:?}"
    );
}

/// The column list of an index definition, excluding its name and any predicate.
fn indexed_columns(def: &str) -> &str {
    let (_, rest) = def
        .split_once(" USING btree (")
        .unwrap_or_else(|| panic!("unexpected index definition: {def}"));
    rest.split_once(')').map_or(rest, |(cols, _)| cols)
}

async fn insert_order(db: &DatabaseConnection, external_ref: &str, status: &str) -> Result<(), DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO orders (customer_id, external_ref, warehouse_id, status, created_at) \
         VALUES (1, $1, 1, $2, now())",
        [external_ref.into(), status.into()],
    ))
    .await
    .map(|_| ())
}

#[tokio::test]
async fn fresh_apply_creates_both_advanced_indexes() {
    let db = connect().await;
    reset(&db).await;

    schema_builder(&db).apply(&db).await.expect("apply()");

    let indexes = indexes_of(&db, "orders").await;
    assert_index(&indexes, "uniq_orders_pending_ref");
    insert_order(&db, "R-1", "pending")
        .await
        .expect("first pending order with a reference");
    assert!(
        insert_order(&db, "R-1", "pending").await.is_err(),
        "a second pending order must not reuse a reference"
    );
    insert_order(&db, "R-1", "paid")
        .await
        .expect("a non-pending order may reuse a pending order's reference");

    let composite = assert_index(&indexes, "idx_orders_status_created");
    assert_eq!(
        indexed_columns(&composite.2),
        "status, created_at",
        "expected a (status, created_at) composite index, got: {}",
        composite.2
    );
    assert!(
        !composite.2.to_uppercase().contains("UNIQUE"),
        "expected a non-unique composite index, got: {}",
        composite.2
    );
    assert_index(&indexes, "idx-orders-warehouse_id");
}

#[tokio::test]
async fn sync_preserves_a_hand_created_partial_unique_index() {
    let db = connect().await;
    reset(&db).await;
    create_hand_written_orders_table(&db, false).await;

    let before = indexes_of(&db, "orders").await;
    let before_oid = assert_index(&before, "uniq_orders_pending_ref").1;

    schema_builder(&db).sync(&db).await.expect("sync() must not fail on a partial unique index");

    let after = indexes_of(&db, "orders").await;
    let after_oid = assert_index(&after, "uniq_orders_pending_ref").1;
    assert_eq!(
        before_oid, after_oid,
        "the partial unique index was dropped and recreated instead of left alone"
    );
}

#[tokio::test]
async fn sync_preserves_a_hand_created_composite_index() {
    let db = connect().await;
    reset(&db).await;
    create_hand_written_orders_table(&db, false).await;

    let before = indexes_of(&db, "orders").await;
    let before_oid = assert_index(&before, "idx_orders_status_created").1;

    schema_builder(&db).sync(&db).await.expect("sync()");

    let after = indexes_of(&db, "orders").await;
    let after_oid = assert_index(&after, "idx_orders_status_created").1;
    assert_eq!(
        before_oid, after_oid,
        "the composite index was dropped and recreated instead of recognized as declared"
    );
}

#[tokio::test]
async fn sync_still_drops_a_genuinely_stale_unique_index() {
    let db = connect().await;
    reset(&db).await;
    create_hand_written_orders_table(&db, true).await;

    schema_builder(&db).sync(&db).await.expect("sync()");

    let after = indexes_of(&db, "orders").await;
    assert_absent(&after, "orders_customer_id_key");
    // the two advanced indexes and the plain control index must still be there
    assert_index(&after, "uniq_orders_pending_ref");
    assert_index(&after, "idx_orders_status_created");
    assert_index(&after, "idx-orders-warehouse_id");
}

#[tokio::test]
async fn sync_drops_a_stale_unique_index_without_a_constraint() {
    let db = connect().await;
    reset(&db).await;
    create_hand_written_orders_table(&db, false).await;
    // A plain unique index has no pg_constraint row, so it cannot be removed with
    // ALTER TABLE ... DROP CONSTRAINT -- even when its name follows the `_key` suffix
    // Postgres uses for constraint-backed indexes.
    db.execute_unprepared("CREATE UNIQUE INDEX orders_customer_id_key ON orders (customer_id);")
        .await
        .expect("add unrelated stale unique index");

    schema_builder(&db)
        .sync(&db)
        .await
        .expect("sync() must not fail on a stale unique index without a constraint");

    let after = indexes_of(&db, "orders").await;
    assert_absent(&after, "orders_customer_id_key");
    assert_index(&after, "uniq_orders_pending_ref");
    assert_index(&after, "idx_orders_status_created");
    assert_index(&after, "idx-orders-warehouse_id");
}

#[tokio::test]
async fn sync_in_a_transaction_drops_a_stale_unique_index_without_a_constraint() {
    let db = connect().await;
    reset(&db).await;
    create_hand_written_orders_table(&db, false).await;
    // Any failed statement aborts a Postgres transaction, so the drop path has to pick
    // the right statement up front rather than recover from a failed attempt.
    db.execute_unprepared("CREATE UNIQUE INDEX uniq_orders_customer_id ON orders (customer_id);")
        .await
        .expect("add unrelated stale unique index");

    let txn = db.begin().await.expect("begin transaction");
    schema_builder(&db)
        .sync(&txn)
        .await
        .expect("sync() inside a transaction");
    txn.commit().await.expect("commit");

    let after = indexes_of(&db, "orders").await;
    assert_absent(&after, "uniq_orders_customer_id");
    assert_index(&after, "uniq_orders_pending_ref");
    assert_index(&after, "idx_orders_status_created");
    assert_index(&after, "idx-orders-warehouse_id");
}

#[tokio::test]
async fn sync_replaces_a_stale_unique_index_on_the_same_columns() {
    let db = connect().await;
    reset(&db).await;
    create_orders_table_without_partial_unique(&db).await;
    // Covers the same column as the declared partial index but is not it. A custom
    // constraint name: only the catalog tells a constraint from a plain index.
    db.execute_unprepared(
        "ALTER TABLE orders ADD CONSTRAINT orders_external_ref_unique UNIQUE (external_ref);",
    )
    .await
    .expect("add stale unique constraint on external_ref");

    let before = indexes_of(&db, "orders").await;
    let composite_oid = assert_index(&before, "idx_orders_status_created").1;
    let plain_oid = assert_index(&before, "idx-orders-warehouse_id").1;

    schema_builder(&db).sync(&db).await.expect("sync()");

    let after = indexes_of(&db, "orders").await;
    assert_absent(&after, "orders_external_ref_unique");
    let unique = assert_index(&after, "uniq_orders_pending_ref");
    assert!(
        unique.2.contains("WHERE"),
        "expected the declared partial unique index, got: {}",
        unique.2
    );
    assert_eq!(assert_index(&after, "idx_orders_status_created").1, composite_oid);
    assert_eq!(assert_index(&after, "idx-orders-warehouse_id").1, plain_oid);
}

#[tokio::test]
async fn sync_replaces_a_partial_unique_index_with_a_drifted_predicate() {
    let db = connect().await;
    reset(&db).await;
    create_orders_table_without_partial_unique(&db).await;
    // Right name and column, wrong predicate.
    db.execute_unprepared(
        "CREATE UNIQUE INDEX uniq_orders_pending_ref ON orders (external_ref) WHERE status = 'paid';",
    )
    .await
    .expect("create drifted partial unique index");

    let before = indexes_of(&db, "orders").await;
    let composite_oid = assert_index(&before, "idx_orders_status_created").1;
    let plain_oid = assert_index(&before, "idx-orders-warehouse_id").1;

    schema_builder(&db).sync(&db).await.expect("sync()");

    insert_order(&db, "R-1", "pending")
        .await
        .expect("first pending order with a reference");
    assert!(
        insert_order(&db, "R-1", "pending").await.is_err(),
        "after sync a second pending order must not reuse a reference"
    );
    insert_order(&db, "R-1", "paid")
        .await
        .expect("a paid order may reuse a pending order's reference");
    insert_order(&db, "R-1", "paid")
        .await
        .expect("paid orders are not constrained by the declared index");

    let after = indexes_of(&db, "orders").await;
    assert_eq!(assert_index(&after, "idx_orders_status_created").1, composite_oid);
    assert_eq!(assert_index(&after, "idx-orders-warehouse_id").1, plain_oid);
}

#[tokio::test]
async fn sync_replaces_a_composite_index_with_drifted_column_order() {
    let db = connect().await;
    reset(&db).await;
    create_bare_orders_table(&db).await;
    // Right name and columns, wrong leading column.
    db.execute_unprepared(
        "CREATE INDEX idx_orders_status_created ON orders (created_at, status); \
         CREATE UNIQUE INDEX uniq_orders_pending_ref ON orders (external_ref) WHERE status = 'pending';",
    )
    .await
    .expect("create drifted composite index");

    let before = indexes_of(&db, "orders").await;
    let partial_oid = assert_index(&before, "uniq_orders_pending_ref").1;
    let plain_oid = assert_index(&before, "idx-orders-warehouse_id").1;

    schema_builder(&db).sync(&db).await.expect("sync()");

    let after = indexes_of(&db, "orders").await;
    let composite = assert_index(&after, "idx_orders_status_created");
    assert_eq!(
        indexed_columns(&composite.2),
        "status, created_at",
        "expected the drifted composite index to be rebuilt as (status, created_at), got: {}",
        composite.2
    );
    assert_eq!(assert_index(&after, "uniq_orders_pending_ref").1, partial_oid);
    assert_eq!(assert_index(&after, "idx-orders-warehouse_id").1, plain_oid);
}

#[tokio::test]
async fn plain_indexed_column_is_unaffected() {
    let db = connect().await;
    reset(&db).await;
    create_hand_written_orders_table(&db, false).await;

    let before = indexes_of(&db, "orders").await;
    let before_oid = assert_index(&before, "idx-orders-warehouse_id").1;

    schema_builder(&db).apply(&db).await.ok();
    schema_builder(&db).sync(&db).await.expect("sync()");
    schema_builder(&db).sync(&db).await.expect("sync() again");

    let after = indexes_of(&db, "orders").await;
    let after_oid = assert_index(&after, "idx-orders-warehouse_id").1;
    assert_eq!(before_oid, after_oid, "the plain indexed column's index was touched");
}
