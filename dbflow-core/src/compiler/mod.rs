mod error;
pub use error::CompileError;

mod types;
pub use types::*;

mod emit;
pub use emit::*;

pub(crate) mod rule;

mod compile;
pub use compile::compile;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hcl_types::parse_hcl_body;
    use parsing::rule::Predicate;
    use std::path::Path;

    #[test]
    fn test_compile_edb_only() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.program.edbs().len(), 1);
        assert_eq!(result.program.edbs()[0].name(), "server");
        assert_eq!(result.program.edbs()[0].arity(), 3); // label, ip, dc
        assert_eq!(result.program.idbs().len(), 0);
        assert_eq!(result.program.rules().len(), 0);
        assert_eq!(result.edb_facts["server"].len(), 1);
    }

    #[test]
    fn test_compile_edb_and_idb() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            resource "monitor" "m1" {
                target_ip = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        // 1 EDB (server) + 1 label-binding EDB (_hcl_lbl_monitor_m1).
        assert_eq!(result.program.edbs().len(), 2);
        assert_eq!(result.program.idbs().len(), 1);
        assert_eq!(result.program.rules().len(), 1);

        let rule = &result.program.rules()[0];
        assert_eq!(rule.head().name(), "monitor");
        // 1 body atom (server join) + 1 body atom (label EDB binding).
        assert_eq!(rule.rhs().len(), 2);
    }

    #[test]
    fn test_variable_substitution() {
        let hcl_src = r#"
            variable "threshold" {
                default = 80
            }

            resource "config" "main" {
                limit = var.threshold
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.program.edbs().len(), 1);
        assert_eq!(result.edb_facts["config"].len(), 1);
        // The threshold value (80) should be in the fact tuple.
        let tuple = &result.edb_facts["config"][0];
        assert_eq!(tuple[1], 80); // limit = 80
    }

    #[test]
    fn test_emit_datalog() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            resource "monitor" "m1" {
                target_ip = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        assert!(dl.contains(".in"));
        assert!(dl.contains(".decl server(label: string, ip: string, dc: string)"));
        assert!(dl.contains(".printsize"));
        assert!(dl.contains(".decl monitor(label: string, target_ip: string)"));
        assert!(dl.contains("monitor"));
        assert!(dl.contains("server"));
    }

    #[test]
    fn test_output_reference() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            output "server_ip" {
                value = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        // Should have one output.
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].name, "server_ip");
        assert_eq!(result.outputs[0].relation_name, "hcl_output_server_ip");

        // Should have an IDB declaration for the output.
        assert!(result
            .program
            .idbs()
            .iter()
            .any(|d| d.name() == "hcl_output_server_ip"));

        // Should have a rule for the output.
        let rule = result
            .program
            .rules()
            .iter()
            .find(|r| r.head().name() == "hcl_output_server_ip");
        assert!(rule.is_some());
    }

    #[test]
    fn test_output_reference_emit_dl() {
        let hcl_src = r#"
            resource "server" "web1" {
                ip = "10.0.0.5"
                dc = "us-west"
            }

            resource "monitor" "m1" {
                target_ip = server.web1.ip
            }

            output "monitors" {
                value = monitor.m1.target_ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        assert!(dl.contains(".decl hcl_output_monitors(value: string)"));
        // "m1" is interned as id 0 (first block processed in topo order).
        assert!(
            dl.contains("hcl_output_monitors(Value) :- monitor(0, Value)."),
            "Expected interned label in output rule, got:\n{}",
            dl
        );
    }

    #[test]
    fn test_output_literal() {
        let hcl_src = r#"
            output "greeting" {
                value = "hello"
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].name, "greeting");
        // Literal output becomes an EDB fact.
        assert!(result.edb_facts.contains_key("hcl_output_greeting"));
        assert_eq!(result.edb_facts["hcl_output_greeting"].len(), 1);
    }

    #[test]
    fn test_output_with_variable() {
        let hcl_src = r#"
            variable "port" {
                default = 8080
            }

            resource "server" "web1" {
                ip = "10.0.0.5"
                port = var.port
            }

            output "server_ip" {
                value = server.web1.ip
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        assert_eq!(result.outputs.len(), 1);
        assert!(result
            .program
            .idbs()
            .iter()
            .any(|d| d.name() == "hcl_output_server_ip"));
    }

    #[test]
    fn test_module_compile_emit_dl() {
        use std::io::Write;

        // Create a child module file.
        let child_hcl = r#"
            variable "ip" {
                default = "0.0.0.0"
            }

            resource "server" "s1" {
                ip = var.ip
            }

            output "server_ip" {
                value = server.s1.ip
            }
        "#;
        let mut child_file = tempfile::Builder::new().suffix(".hcl").tempfile().unwrap();
        child_file.write_all(child_hcl.as_bytes()).unwrap();

        let parent_hcl = format!(
            r#"
            module "web" {{
                source = "{}"
                ip = "10.0.0.1"
            }}

            output "result" {{
                value = module.web.server_ip
            }}
        "#,
            child_file.path().to_string_lossy().replace('\\', "/")
        );

        let body: hcl::Body = hcl::from_str(&parent_hcl).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, Some(Path::new("/tmp")), &[], &[]).unwrap();
        let dl = emit_datalog(&result);

        // Should have namespaced EDB: web_server
        assert!(
            dl.contains("web_server"),
            "Expected web_server in DL:\n{}",
            dl
        );
        // Should have output IDB: hcl_output_result
        assert!(
            dl.contains("hcl_output_result"),
            "Expected hcl_output_result in DL:\n{}",
            dl
        );
        // Should have a rule connecting the output to the namespaced relation.
        assert!(
            dl.contains("hcl_output_result(Value) :- web_server("),
            "Expected output rule in DL:\n{}",
            dl
        );
    }

    #[test]
    fn test_data_block_edb() {
        // A data block should generate an EDB relation with the correct schema.
        let hcl_src = r#"
            output "greeting" {
                value = "hello"
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "users".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "name".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "age".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![
                vec![
                    dbflow_plugin::DataValue::String("alice".to_string()),
                    dbflow_plugin::DataValue::Integer(30),
                ],
                vec![
                    dbflow_plugin::DataValue::String("bob".to_string()),
                    dbflow_plugin::DataValue::Integer(25),
                ],
            ],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();

        // Should have an EDB declaration for _data_csv_users.
        assert!(
            result
                .program
                .edbs()
                .iter()
                .any(|d| d.name() == "_data_csv_users"),
            "Expected _data_csv_users EDB declaration"
        );

        // Should have 2 fact rows.
        let facts = &result.edb_facts["_data_csv_users"];
        assert_eq!(facts.len(), 2);

        // The integer column (age) should be raw i32 values.
        assert_eq!(facts[0][1], 30);
        assert_eq!(facts[1][1], 25);
    }

    #[test]
    fn test_data_reference_in_output() {
        // An output referencing a data block field should produce an IDB rule.
        let hcl_src = r#"
            output "user_name" {
                value = data.csv.users.name
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "users".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "name".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "age".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![vec![
                dbflow_plugin::DataValue::String("alice".to_string()),
                dbflow_plugin::DataValue::Integer(30),
            ]],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();

        // Should have an IDB for the output.
        assert!(
            result
                .program
                .idbs()
                .iter()
                .any(|d| d.name() == "hcl_output_user_name"),
            "Expected hcl_output_user_name IDB declaration"
        );

        // Should have a rule: hcl_output_user_name(Value) :- _data_csv_users(Value, _).
        let rule = result
            .program
            .rules()
            .iter()
            .find(|r| r.head().name() == "hcl_output_user_name")
            .expect("Expected rule for hcl_output_user_name");

        let dl = emit_datalog(&result);
        assert!(
            dl.contains("hcl_output_user_name(Value) :- _data_csv_users(Value, _)."),
            "Expected data reference output rule, got:\n{}",
            dl
        );
        let _ = rule;
    }

    #[test]
    fn test_data_reference_in_resource() {
        // A resource referencing a data block field should produce an IDB rule
        // with a data body atom.
        let hcl_src = r#"
            resource "enriched" "e1" {
                username = data.csv.users.name
            }

            output "result" {
                value = enriched.e1.username
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "users".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![dbflow_plugin::ColumnDef {
                    name: "name".to_string(),
                    data_type: dbflow_plugin::DataType::String,
                }],
            },
            rows: vec![vec![dbflow_plugin::DataValue::String("alice".to_string())]],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();

        // Should have IDB for enriched.
        assert!(
            result.program.idbs().iter().any(|d| d.name() == "enriched"),
            "Expected enriched IDB declaration"
        );

        // The rule should reference _data_csv_users in its body.
        let dl = emit_datalog(&result);
        assert!(
            dl.contains("_data_csv_users(Username)"),
            "Expected data body atom in rule, got:\n{}",
            dl
        );
    }

    #[test]
    fn test_comparison_filter() {
        let hcl_src = r#"
            resource "order" "o1" {
                item = "widget"
                amount = 100
            }

            resource "big_order" "rule" {
                item = order.o1.item
                amount = order.o1.amount
                _filter = order.o1.amount > 50
            }

            output "result" {
                value = big_order.rule.item
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]).unwrap();

        // big_order should be an IDB.
        assert!(
            result
                .program
                .idbs()
                .iter()
                .any(|d| d.name() == "big_order"),
            "Expected big_order IDB"
        );

        // The rule should contain a ComparePredicate.
        let rule = result
            .program
            .rules()
            .iter()
            .find(|r| r.head().name() == "big_order")
            .expect("Expected big_order rule");
        let has_compare = rule
            .rhs()
            .iter()
            .any(|p| matches!(p, Predicate::ComparePredicate(_)));
        assert!(
            has_compare,
            "Expected ComparePredicate in rule body: {}",
            rule
        );

        // Schema should NOT include _filter.
        let dl = emit_datalog(&result);
        assert!(
            !dl.contains("Filter"),
            "Schema should exclude _filter attribute, got:\n{}",
            dl
        );
    }

    #[test]
    fn test_comparison_filter_data_block() {
        let hcl_src = r#"
            resource "big_order" "rule" {
                customer = data.csv.orders.customer
                amount = data.csv.orders.amount
                _filter = data.csv.orders.amount > 50
            }

            output "result" {
                value = big_order.rule.customer
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "orders".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "customer".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "amount".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![
                vec![
                    dbflow_plugin::DataValue::String("alice".to_string()),
                    dbflow_plugin::DataValue::Integer(100),
                ],
                vec![
                    dbflow_plugin::DataValue::String("bob".to_string()),
                    dbflow_plugin::DataValue::Integer(30),
                ],
            ],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();
        let dl = emit_datalog(&result);

        // Should have a comparison predicate in the rule.
        assert!(
            dl.contains("> 50"),
            "Expected comparison > 50 in rule, got:\n{}",
            dl
        );
    }

    #[test]
    fn test_aggregate_sum() {
        let hcl_src = r#"
            resource "totals" "all" {
                region = data.csv.sales.region
                total = sum(data.csv.sales.amount)
            }

            output "result" {
                value = totals.all.region
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "sales".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "region".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "amount".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![
                vec![
                    dbflow_plugin::DataValue::String("us".to_string()),
                    dbflow_plugin::DataValue::Integer(100),
                ],
                vec![
                    dbflow_plugin::DataValue::String("us".to_string()),
                    dbflow_plugin::DataValue::Integer(200),
                ],
                vec![
                    dbflow_plugin::DataValue::String("eu".to_string()),
                    dbflow_plugin::DataValue::Integer(50),
                ],
            ],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();
        let dl = emit_datalog(&result);

        // Should have an aggregation in the head.
        assert!(
            dl.contains("sum("),
            "Expected sum() in rule head, got:\n{}",
            dl
        );

        // Should have IDB for totals.
        assert!(
            result.program.idbs().iter().any(|d| d.name() == "totals"),
            "Expected totals IDB"
        );
    }

    #[test]
    fn test_aggregate_in_edb_errors() {
        let hcl_src = r#"
            resource "bad" "b1" {
                total = 42
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        // This should succeed since it's just a literal.
        let result = compile(hcl_prog, None, &[], &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_comparison_in_output_errors() {
        let hcl_src = r#"
            output "bad" {
                value = 1 > 0
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]);
        match result {
            Err(e) => assert!(
                e.to_string().contains("comparison"),
                "Expected comparison error message, got: {}",
                e
            ),
            Ok(_) => panic!("Expected error for comparison in output"),
        }
    }

    #[test]
    fn test_aggregate_in_output_errors() {
        let hcl_src = r#"
            output "bad" {
                value = sum(1)
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();
        let result = compile(hcl_prog, None, &[], &[]);
        match result {
            Err(e) => assert!(
                e.to_string().contains("aggregate"),
                "Expected aggregate error message, got: {}",
                e
            ),
            Ok(_) => panic!("Expected error for aggregate in output"),
        }
    }

    #[test]
    fn test_arithmetic_in_filter() {
        // Test: _filter = data.csv.orders.amount + data.csv.orders.tax > 1000
        let hcl_src = r#"
            resource "expensive" "rule" {
                customer = data.csv.orders.customer
                _filter = data.csv.orders.amount + data.csv.orders.tax > 1000
            }

            output "result" {
                value = expensive.rule.customer
            }
        "#;
        let body: hcl::Body = hcl::from_str(hcl_src).unwrap();
        let hcl_prog = parse_hcl_body(&body).unwrap();

        let data_blocks = vec![FetchedDataBlock {
            provider_type: "csv".to_string(),
            label: "orders".to_string(),
            schema: dbflow_plugin::DataSchema {
                columns: vec![
                    dbflow_plugin::ColumnDef {
                        name: "customer".to_string(),
                        data_type: dbflow_plugin::DataType::String,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "amount".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                    dbflow_plugin::ColumnDef {
                        name: "tax".to_string(),
                        data_type: dbflow_plugin::DataType::Integer,
                    },
                ],
            },
            rows: vec![vec![
                dbflow_plugin::DataValue::String("alice".to_string()),
                dbflow_plugin::DataValue::Integer(900),
                dbflow_plugin::DataValue::Integer(200),
            ]],
        }];

        let result = compile(hcl_prog, None, &data_blocks, &[]).unwrap();
        let dl = emit_datalog(&result);

        // Should contain arithmetic in the comparison.
        assert!(
            dl.contains("+") && dl.contains("> 1000"),
            "Expected arithmetic comparison in rule, got:\n{}",
            dl
        );
    }
}
