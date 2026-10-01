use sea_query::{Expr, ExprTrait, OnConflict, Query};

use crate::{Db, Result};

#[derive(sqlx::FromRow)]
struct ValueRow {
    value: String,
}

impl Db {
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let q = Query::select()
            .column("value")
            .from("settings")
            .and_where(Expr::col("key").eq(key))
            .to_owned();
        Ok(self.fetch_optional::<ValueRow>(&q).await?.map(|r| r.value))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let q = Query::insert()
            .into_table("settings")
            .columns(["key", "value"])
            .values_panic([key.into(), value.into()])
            .on_conflict(OnConflict::column("key").update_column("value").to_owned())
            .to_owned();
        self.execute(&q).await?;
        Ok(())
    }
}
