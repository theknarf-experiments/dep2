use std::collections::HashMap;

use dbflow_plugin::{
    ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue, Plugin, PluginContext,
};

pub struct CsvPlugin;

impl Plugin for CsvPlugin {
    fn name(&self) -> &str {
        "csv"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_data_provider(Box::new(CsvDataProvider));
    }
}

struct CsvDataProvider;

impl DataProvider for CsvDataProvider {
    fn name(&self) -> &str {
        "csv"
    }

    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String> {
        let path = config
            .get("path")
            .ok_or("csv data provider requires 'path' config attribute")?;

        let mut reader = csv::Reader::from_path(path)
            .map_err(|e| format!("failed to open CSV '{}': {}", path, e))?;

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| format!("failed to read CSV headers: {}", e))?
            .iter()
            .map(|h| h.to_string())
            .collect();

        if headers.is_empty() {
            return Err("CSV file has no columns".to_string());
        }

        let schema = DataSchema {
            columns: headers
                .iter()
                .map(|name| ColumnDef {
                    name: name.clone(),
                    data_type: DataType::String,
                })
                .collect(),
        };

        let mut rows = Vec::new();
        for result in reader.records() {
            let record = result.map_err(|e| format!("CSV parse error: {}", e))?;
            let row: Vec<DataValue> = record
                .iter()
                .map(|field| DataValue::String(field.to_string()))
                .collect();
            rows.push(row);
        }

        Ok(Box::new(CsvDataSource { schema, rows }))
    }
}

struct CsvDataSource {
    schema: DataSchema,
    rows: Vec<Vec<DataValue>>,
}

impl DataSource for CsvDataSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn fetch_all(&self) -> Result<Vec<Vec<DataValue>>, String> {
        Ok(self.rows.clone())
    }
}
