pub const V1: usize = 1;
pub const V2: usize = 2;

pub fn migrate() {
    // Create the view for model_catalog
    let view_sql = "CREATE VIEW model_catalog AS SELECT * FROM fleet_model_catalog";
    execute_sql(view_sql);
}
