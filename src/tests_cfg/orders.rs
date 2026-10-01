//! An entity definition for testing schema-sync of indexes the entity macro cannot
//! express through column-level attributes alone: a partial predicate, an explicit
//! composite column order, or a non-unique composite index. See sea-orm#3157.
use crate as sea_orm;
use crate::entity::prelude::*;
use crate::sea_query::IndexCreateStatement;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "orders")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub customer_id: i32,
    pub external_ref: Option<String>,
    #[sea_orm(indexed)]
    pub warehouse_id: i32,
    pub status: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Indexes schema-sync cannot derive from `Model`'s column-level attributes alone.
    /// See the `postgres` MCP server for the reference schema this must match.
    pub fn extra_indexes() -> Vec<IndexCreateStatement> {
        use crate::sea_query::{ConditionalStatement, Expr, ExprTrait, Index};

        vec![
            Index::create()
                .name("uniq_orders_pending_ref")
                .table(Entity)
                .col(Column::ExternalRef)
                .unique()
                .and_where(Expr::col(Column::Status).eq("pending"))
                .take(),
            Index::create()
                .name("idx_orders_status_created")
                .table(Entity)
                .col(Column::Status)
                .col(Column::CreatedAt)
                .take(),
        ]
    }
}
