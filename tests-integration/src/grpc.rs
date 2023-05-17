// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use api::v1::column::{SemanticType, Values};
    use api::v1::ddl_request::Expr as DdlExpr;
    use api::v1::greptime_request::Request;
    use api::v1::query_request::Query;
    use api::v1::{
        alter_expr, AddColumn, AddColumns, AlterExpr, Column, ColumnDataType, ColumnDef,
        CreateDatabaseExpr, CreateTableExpr, DdlRequest, DeleteRequest, DropTableExpr,
        FlushTableExpr, InsertRequest, QueryRequest,
    };
    use catalog::helper::{TableGlobalKey, TableGlobalValue};
    use common_catalog::consts::MITO_ENGINE;
    use common_query::Output;
    use common_recordbatch::RecordBatches;
    use frontend::instance::Instance;
    use frontend::table::DistTable;
    use query::parser::QueryLanguageParser;
    use servers::query_handler::grpc::GrpcQueryHandler;
    use session::context::QueryContext;
    use tests::{has_parquet_file, test_region_dir};

    use crate::tests;
    use crate::tests::MockDistributedInstance;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_distributed_handle_ddl_request() {
        let instance =
            tests::create_distributed_instance("test_distributed_handle_ddl_request").await;
        let frontend = &instance.frontend;

        test_handle_ddl_request(frontend.as_ref()).await;

        verify_table_is_dropped(&instance).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_handle_ddl_request() {
        let standalone =
            tests::create_standalone_instance("test_standalone_handle_ddl_request").await;
        let instance = &standalone.instance;

        test_handle_ddl_request(instance.as_ref()).await;
    }

    async fn query(instance: &Instance, request: Request) -> Output {
        GrpcQueryHandler::do_query(instance, request, QueryContext::arc())
            .await
            .unwrap()
    }

    async fn test_handle_ddl_request(instance: &Instance) {
        let request = Request::Ddl(DdlRequest {
            expr: Some(DdlExpr::CreateDatabase(CreateDatabaseExpr {
                database_name: "database_created_through_grpc".to_string(),
                create_if_not_exists: true,
            })),
        });
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(1)));

        let request = Request::Ddl(DdlRequest {
            expr: Some(DdlExpr::CreateTable(CreateTableExpr {
                catalog_name: "greptime".to_string(),
                schema_name: "database_created_through_grpc".to_string(),
                table_name: "table_created_through_grpc".to_string(),
                column_defs: vec![
                    ColumnDef {
                        name: "a".to_string(),
                        datatype: ColumnDataType::String as _,
                        is_nullable: true,
                        default_constraint: vec![],
                    },
                    ColumnDef {
                        name: "ts".to_string(),
                        datatype: ColumnDataType::TimestampMillisecond as _,
                        is_nullable: false,
                        default_constraint: vec![],
                    },
                ],
                time_index: "ts".to_string(),
                engine: MITO_ENGINE.to_string(),
                ..Default::default()
            })),
        });
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(0)));

        let request = Request::Ddl(DdlRequest {
            expr: Some(DdlExpr::Alter(AlterExpr {
                catalog_name: "greptime".to_string(),
                schema_name: "database_created_through_grpc".to_string(),
                table_name: "table_created_through_grpc".to_string(),
                kind: Some(alter_expr::Kind::AddColumns(AddColumns {
                    add_columns: vec![AddColumn {
                        column_def: Some(ColumnDef {
                            name: "b".to_string(),
                            datatype: ColumnDataType::Int32 as _,
                            is_nullable: true,
                            default_constraint: vec![],
                        }),
                        is_key: false,
                    }],
                })),
            })),
        });
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(0)));

        let request = Request::Query(QueryRequest {
            query: Some(Query::Sql("INSERT INTO database_created_through_grpc.table_created_through_grpc (a, b, ts) VALUES ('s', 1, 1672816466000)".to_string()))
        });
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(1)));

        let request = Request::Query(QueryRequest {
            query: Some(Query::Sql(
                "SELECT ts, a, b FROM database_created_through_grpc.table_created_through_grpc"
                    .to_string(),
            )),
        });
        let output = query(instance, request).await;
        let Output::Stream(stream) = output else { unreachable!() };
        let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
        let expected = "\
+---------------------+---+---+
| ts                  | a | b |
+---------------------+---+---+
| 2023-01-04T07:14:26 | s | 1 |
+---------------------+---+---+";
        assert_eq!(recordbatches.pretty_print().unwrap(), expected);

        let request = Request::Ddl(DdlRequest {
            expr: Some(DdlExpr::DropTable(DropTableExpr {
                catalog_name: "greptime".to_string(),
                schema_name: "database_created_through_grpc".to_string(),
                table_name: "table_created_through_grpc".to_string(),
            })),
        });
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(1)));
    }

    async fn verify_table_is_dropped(instance: &MockDistributedInstance) {
        for (_, dn) in instance.datanodes.iter() {
            assert!(dn
                .catalog_manager()
                .table(
                    "greptime",
                    "database_created_through_grpc",
                    "table_created_through_grpc"
                )
                .await
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_distributed_insert_delete_and_query() {
        common_telemetry::init_default_ut_logging();

        let instance =
            tests::create_distributed_instance("test_distributed_insert_delete_and_query").await;
        let frontend = instance.frontend.as_ref();

        let table_name = "my_dist_table";
        let sql = format!(
            r"
CREATE TABLE {table_name} (
    a INT,
    b STRING PRIMARY KEY,
    ts TIMESTAMP,
    TIME INDEX (ts)
) PARTITION BY RANGE COLUMNS(a) (
    PARTITION r0 VALUES LESS THAN (10),
    PARTITION r1 VALUES LESS THAN (20),
    PARTITION r2 VALUES LESS THAN (50),
    PARTITION r3 VALUES LESS THAN (MAXVALUE),
)"
        );
        create_table(frontend, sql).await;

        test_insert_delete_and_query_on_existing_table(frontend, table_name).await;

        verify_data_distribution(
            &instance,
            table_name,
            HashMap::from([
                (
                    0u32,
                    "\
+---------------------+---+-------------------+
| ts                  | a | b                 |
+---------------------+---+-------------------+
| 2023-01-01T07:26:12 | 1 | ts: 1672557972000 |
| 2023-01-01T07:26:14 | 3 | ts: 1672557974000 |
| 2023-01-01T07:26:15 | 4 | ts: 1672557975000 |
| 2023-01-01T07:26:16 | 5 | ts: 1672557976000 |
| 2023-01-01T07:26:17 |   | ts: 1672557977000 |
+---------------------+---+-------------------+",
                ),
                (
                    1u32,
                    "\
+---------------------+----+-------------------+
| ts                  | a  | b                 |
+---------------------+----+-------------------+
| 2023-01-01T07:26:18 | 11 | ts: 1672557978000 |
+---------------------+----+-------------------+",
                ),
                (
                    2u32,
                    "\
+---------------------+----+-------------------+
| ts                  | a  | b                 |
+---------------------+----+-------------------+
| 2023-01-01T07:26:20 | 20 | ts: 1672557980000 |
| 2023-01-01T07:26:21 | 21 | ts: 1672557981000 |
| 2023-01-01T07:26:23 | 23 | ts: 1672557983000 |
+---------------------+----+-------------------+",
                ),
                (
                    3u32,
                    "\
+---------------------+----+-------------------+
| ts                  | a  | b                 |
+---------------------+----+-------------------+
| 2023-01-01T07:26:24 | 50 | ts: 1672557984000 |
| 2023-01-01T07:26:25 | 51 | ts: 1672557985000 |
| 2023-01-01T07:26:27 | 53 | ts: 1672557987000 |
+---------------------+----+-------------------+",
                ),
            ]),
        )
        .await;

        test_insert_delete_and_query_on_auto_created_table(frontend).await;

        // Auto created table has only one region.
        verify_data_distribution(
            &instance,
            "auto_created_table",
            HashMap::from([(
                0u32,
                "\
+---------------------+---+---+
| ts                  | a | b |
+---------------------+---+---+
| 2023-01-01T07:26:16 |   |   |
| 2023-01-01T07:26:17 | 6 |   |
| 2023-01-01T07:26:18 |   | x |
| 2023-01-01T07:26:20 |   | z |
+---------------------+---+---+",
            )]),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_insert_and_query() {
        common_telemetry::init_default_ut_logging();

        let standalone =
            tests::create_standalone_instance("test_standalone_insert_and_query").await;
        let instance = &standalone.instance;

        let table_name = "my_table";
        let sql = format!("CREATE TABLE {table_name} (a INT, b STRING, ts TIMESTAMP, TIME INDEX (ts), PRIMARY KEY (a, b))");
        create_table(instance, sql).await;

        test_insert_delete_and_query_on_existing_table(instance, table_name).await;

        test_insert_delete_and_query_on_auto_created_table(instance).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_distributed_flush_table() {
        common_telemetry::init_default_ut_logging();

        let instance = tests::create_distributed_instance("test_distributed_flush_table").await;
        let data_tmp_dirs = instance.data_tmp_dirs();
        let frontend = instance.frontend.as_ref();

        let table_name = "my_dist_table";
        let sql = format!(
            r"
CREATE TABLE {table_name} (
    a INT,
    ts TIMESTAMP,
    TIME INDEX (ts)
) PARTITION BY RANGE COLUMNS(a) (
    PARTITION r0 VALUES LESS THAN (10),
    PARTITION r1 VALUES LESS THAN (20),
    PARTITION r2 VALUES LESS THAN (50),
    PARTITION r3 VALUES LESS THAN (MAXVALUE),
)"
        );
        create_table(frontend, sql).await;

        test_insert_delete_and_query_on_existing_table(frontend, table_name).await;

        flush_table(frontend, "greptime", "public", table_name, None).await;
        // Wait for previous task finished
        flush_table(frontend, "greptime", "public", table_name, None).await;

        let table = instance
            .frontend
            .catalog_manager()
            .table("greptime", "public", table_name)
            .await
            .unwrap()
            .unwrap();
        let table = table.as_any().downcast_ref::<DistTable>().unwrap();

        let tgv = table
            .table_global_value(&TableGlobalKey {
                catalog_name: "greptime".to_string(),
                schema_name: "public".to_string(),
                table_name: table_name.to_string(),
            })
            .await
            .unwrap()
            .unwrap();
        let table_id = tgv.table_id();

        let region_to_dn_map = tgv
            .regions_id_map
            .iter()
            .map(|(k, v)| (v[0], *k))
            .collect::<HashMap<u32, u64>>();

        for (region, dn) in region_to_dn_map.iter() {
            // data_tmp_dirs -> dn: 1..4
            let data_tmp_dir = data_tmp_dirs.get((*dn - 1) as usize).unwrap();
            let region_dir = test_region_dir(
                data_tmp_dir.path().to_str().unwrap(),
                "greptime",
                "public",
                table_id,
                *region,
            );
            has_parquet_file(&region_dir);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_flush_table() {
        common_telemetry::init_default_ut_logging();

        let standalone = tests::create_standalone_instance("test_standalone_flush_table").await;
        let instance = &standalone.instance;
        let data_tmp_dir = standalone.data_tmp_dir();

        let table_name = "my_table";
        let sql = format!("CREATE TABLE {table_name} (a INT, b STRING, ts TIMESTAMP, TIME INDEX (ts), PRIMARY KEY (a, b))");

        create_table(instance, sql).await;

        test_insert_delete_and_query_on_existing_table(instance, table_name).await;

        let table_id = 1024;
        let region_id = 0;
        let region_dir = test_region_dir(
            data_tmp_dir.path().to_str().unwrap(),
            "greptime",
            "public",
            table_id,
            region_id,
        );
        assert!(!has_parquet_file(&region_dir));

        flush_table(instance, "greptime", "public", "my_table", None).await;
        // Wait for previous task finished
        flush_table(instance, "greptime", "public", "my_table", None).await;

        assert!(has_parquet_file(&region_dir));
    }

    async fn create_table(frontend: &Instance, sql: String) {
        let request = Request::Query(QueryRequest {
            query: Some(Query::Sql(sql)),
        });
        let output = query(frontend, request).await;
        assert!(matches!(output, Output::AffectedRows(0)));
    }

    async fn flush_table(
        frontend: &Instance,
        catalog_name: &str,
        schema_name: &str,
        table_name: &str,
        region_id: Option<u32>,
    ) {
        let request = Request::Ddl(DdlRequest {
            expr: Some(DdlExpr::FlushTable(FlushTableExpr {
                catalog_name: catalog_name.to_string(),
                schema_name: schema_name.to_string(),
                table_name: table_name.to_string(),
                region_id,
            })),
        });

        let output = query(frontend, request).await;
        assert!(matches!(output, Output::AffectedRows(0)));
    }

    async fn test_insert_delete_and_query_on_existing_table(instance: &Instance, table_name: &str) {
        let ts_millisecond_values = vec![
            1672557972000,
            1672557973000,
            1672557974000,
            1672557975000,
            1672557976000,
            1672557977000,
            1672557978000,
            1672557979000,
            1672557980000,
            1672557981000,
            1672557982000,
            1672557983000,
            1672557984000,
            1672557985000,
            1672557986000,
            1672557987000,
        ];
        let insert = InsertRequest {
            table_name: table_name.to_string(),
            columns: vec![
                Column {
                    column_name: "a".to_string(),
                    values: Some(Values {
                        i32_values: vec![1, 2, 3, 4, 5, 11, 12, 20, 21, 22, 23, 50, 51, 52, 53],
                        ..Default::default()
                    }),
                    null_mask: vec![32, 0],
                    semantic_type: SemanticType::Field as i32,
                    datatype: ColumnDataType::Int32 as i32,
                },
                Column {
                    column_name: "b".to_string(),
                    values: Some(Values {
                        string_values: ts_millisecond_values
                            .iter()
                            .map(|x| format!("ts: {x}"))
                            .collect(),
                        ..Default::default()
                    }),
                    semantic_type: SemanticType::Tag as i32,
                    datatype: ColumnDataType::String as i32,
                    ..Default::default()
                },
                Column {
                    column_name: "ts".to_string(),
                    values: Some(Values {
                        ts_millisecond_values,
                        ..Default::default()
                    }),
                    semantic_type: SemanticType::Timestamp as i32,
                    datatype: ColumnDataType::TimestampMillisecond as i32,
                    ..Default::default()
                },
            ],
            row_count: 16,
            ..Default::default()
        };
        let output = query(instance, Request::Insert(insert)).await;
        assert!(matches!(output, Output::AffectedRows(16)));

        let request = Request::Query(QueryRequest {
            query: Some(Query::Sql(format!(
                "SELECT ts, a, b FROM {table_name} ORDER BY ts"
            ))),
        });
        let output = query(instance, request.clone()).await;
        let Output::Stream(stream) = output else { unreachable!() };
        let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
        let expected = "\
+---------------------+----+-------------------+
| ts                  | a  | b                 |
+---------------------+----+-------------------+
| 2023-01-01T07:26:12 | 1  | ts: 1672557972000 |
| 2023-01-01T07:26:13 | 2  | ts: 1672557973000 |
| 2023-01-01T07:26:14 | 3  | ts: 1672557974000 |
| 2023-01-01T07:26:15 | 4  | ts: 1672557975000 |
| 2023-01-01T07:26:16 | 5  | ts: 1672557976000 |
| 2023-01-01T07:26:17 |    | ts: 1672557977000 |
| 2023-01-01T07:26:18 | 11 | ts: 1672557978000 |
| 2023-01-01T07:26:19 | 12 | ts: 1672557979000 |
| 2023-01-01T07:26:20 | 20 | ts: 1672557980000 |
| 2023-01-01T07:26:21 | 21 | ts: 1672557981000 |
| 2023-01-01T07:26:22 | 22 | ts: 1672557982000 |
| 2023-01-01T07:26:23 | 23 | ts: 1672557983000 |
| 2023-01-01T07:26:24 | 50 | ts: 1672557984000 |
| 2023-01-01T07:26:25 | 51 | ts: 1672557985000 |
| 2023-01-01T07:26:26 | 52 | ts: 1672557986000 |
| 2023-01-01T07:26:27 | 53 | ts: 1672557987000 |
+---------------------+----+-------------------+";
        assert_eq!(recordbatches.pretty_print().unwrap(), expected);

        let delete = DeleteRequest {
            table_name: table_name.to_string(),
            region_number: 0,
            key_columns: vec![
                Column {
                    column_name: "a".to_string(),
                    semantic_type: SemanticType::Field as i32,
                    values: Some(Values {
                        i32_values: vec![2, 12, 22, 52],
                        ..Default::default()
                    }),
                    datatype: ColumnDataType::Int32 as i32,
                    ..Default::default()
                },
                Column {
                    column_name: "b".to_string(),
                    semantic_type: SemanticType::Tag as i32,
                    values: Some(Values {
                        string_values: vec![
                            "ts: 1672557973000".to_string(),
                            "ts: 1672557979000".to_string(),
                            "ts: 1672557982000".to_string(),
                            "ts: 1672557986000".to_string(),
                        ],
                        ..Default::default()
                    }),
                    datatype: ColumnDataType::String as i32,
                    ..Default::default()
                },
                Column {
                    column_name: "ts".to_string(),
                    semantic_type: SemanticType::Timestamp as i32,
                    values: Some(Values {
                        ts_millisecond_values: vec![
                            1672557973000,
                            1672557979000,
                            1672557982000,
                            1672557986000,
                        ],
                        ..Default::default()
                    }),
                    datatype: ColumnDataType::TimestampMillisecond as i32,
                    ..Default::default()
                },
            ],
            row_count: 4,
        };
        let output = query(instance, Request::Delete(delete)).await;
        assert!(matches!(output, Output::AffectedRows(4)));

        let output = query(instance, request).await;
        let Output::Stream(stream) = output else { unreachable!() };
        let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
        let expected = "\
+---------------------+----+-------------------+
| ts                  | a  | b                 |
+---------------------+----+-------------------+
| 2023-01-01T07:26:12 | 1  | ts: 1672557972000 |
| 2023-01-01T07:26:14 | 3  | ts: 1672557974000 |
| 2023-01-01T07:26:15 | 4  | ts: 1672557975000 |
| 2023-01-01T07:26:16 | 5  | ts: 1672557976000 |
| 2023-01-01T07:26:17 |    | ts: 1672557977000 |
| 2023-01-01T07:26:18 | 11 | ts: 1672557978000 |
| 2023-01-01T07:26:20 | 20 | ts: 1672557980000 |
| 2023-01-01T07:26:21 | 21 | ts: 1672557981000 |
| 2023-01-01T07:26:23 | 23 | ts: 1672557983000 |
| 2023-01-01T07:26:24 | 50 | ts: 1672557984000 |
| 2023-01-01T07:26:25 | 51 | ts: 1672557985000 |
| 2023-01-01T07:26:27 | 53 | ts: 1672557987000 |
+---------------------+----+-------------------+";
        assert_eq!(recordbatches.pretty_print().unwrap(), expected);
    }

    async fn verify_data_distribution(
        instance: &MockDistributedInstance,
        table_name: &str,
        expected_distribution: HashMap<u32, &str>,
    ) {
        let table = instance
            .frontend
            .catalog_manager()
            .table("greptime", "public", table_name)
            .await
            .unwrap()
            .unwrap();
        let table = table.as_any().downcast_ref::<DistTable>().unwrap();

        let TableGlobalValue { regions_id_map, .. } = table
            .table_global_value(&TableGlobalKey {
                catalog_name: "greptime".to_string(),
                schema_name: "public".to_string(),
                table_name: table_name.to_string(),
            })
            .await
            .unwrap()
            .unwrap();
        let region_to_dn_map = regions_id_map
            .iter()
            .map(|(k, v)| (v[0], *k))
            .collect::<HashMap<u32, u64>>();
        assert_eq!(region_to_dn_map.len(), expected_distribution.len());

        for (region, dn) in region_to_dn_map.iter() {
            let stmt = QueryLanguageParser::parse_sql(&format!(
                "SELECT ts, a, b FROM {table_name} ORDER BY ts"
            ))
            .unwrap();
            let dn = instance.datanodes.get(dn).unwrap();
            let engine = dn.query_engine();
            let plan = engine
                .planner()
                .plan(stmt, QueryContext::arc())
                .await
                .unwrap();
            let output = engine.execute(plan, QueryContext::arc()).await.unwrap();
            let Output::Stream(stream) = output else { unreachable!() };
            let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
            let actual = recordbatches.pretty_print().unwrap();

            let expected = expected_distribution.get(region).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    async fn test_insert_delete_and_query_on_auto_created_table(instance: &Instance) {
        let insert = InsertRequest {
            table_name: "auto_created_table".to_string(),
            columns: vec![
                Column {
                    column_name: "a".to_string(),
                    values: Some(Values {
                        i32_values: vec![4, 6],
                        ..Default::default()
                    }),
                    null_mask: vec![2],
                    semantic_type: SemanticType::Field as i32,
                    datatype: ColumnDataType::Int32 as i32,
                },
                Column {
                    column_name: "ts".to_string(),
                    values: Some(Values {
                        ts_millisecond_values: vec![1672557975000, 1672557976000, 1672557977000],
                        ..Default::default()
                    }),
                    semantic_type: SemanticType::Timestamp as i32,
                    datatype: ColumnDataType::TimestampMillisecond as i32,
                    ..Default::default()
                },
            ],
            row_count: 3,
            ..Default::default()
        };

        // Test auto create not existed table upon insertion.
        let request = Request::Insert(insert);
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(3)));

        let insert = InsertRequest {
            table_name: "auto_created_table".to_string(),
            columns: vec![
                Column {
                    column_name: "b".to_string(),
                    values: Some(Values {
                        string_values: vec!["x".to_string(), "z".to_string()],
                        ..Default::default()
                    }),
                    null_mask: vec![2],
                    semantic_type: SemanticType::Field as i32,
                    datatype: ColumnDataType::String as i32,
                },
                Column {
                    column_name: "ts".to_string(),
                    values: Some(Values {
                        ts_millisecond_values: vec![1672557978000, 1672557979000, 1672557980000],
                        ..Default::default()
                    }),
                    semantic_type: SemanticType::Timestamp as i32,
                    datatype: ColumnDataType::TimestampMillisecond as i32,
                    ..Default::default()
                },
            ],
            row_count: 3,
            ..Default::default()
        };

        // Test auto add not existed column upon insertion.
        let request = Request::Insert(insert);
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(3)));

        let request = Request::Query(QueryRequest {
            query: Some(Query::Sql(
                "SELECT ts, a, b FROM auto_created_table".to_string(),
            )),
        });
        let output = query(instance, request.clone()).await;
        let Output::Stream(stream) = output else { unreachable!() };
        let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
        let expected = "\
+---------------------+---+---+
| ts                  | a | b |
+---------------------+---+---+
| 2023-01-01T07:26:15 | 4 |   |
| 2023-01-01T07:26:16 |   |   |
| 2023-01-01T07:26:17 | 6 |   |
| 2023-01-01T07:26:18 |   | x |
| 2023-01-01T07:26:19 |   |   |
| 2023-01-01T07:26:20 |   | z |
+---------------------+---+---+";
        assert_eq!(recordbatches.pretty_print().unwrap(), expected);

        let delete = DeleteRequest {
            table_name: "auto_created_table".to_string(),
            region_number: 0,
            key_columns: vec![Column {
                column_name: "ts".to_string(),
                values: Some(Values {
                    ts_millisecond_values: vec![1672557975000, 1672557979000],
                    ..Default::default()
                }),
                semantic_type: SemanticType::Timestamp as i32,
                datatype: ColumnDataType::TimestampMillisecond as i32,
                ..Default::default()
            }],
            row_count: 2,
        };

        let output = query(instance, Request::Delete(delete)).await;
        assert!(matches!(output, Output::AffectedRows(2)));

        let output = query(instance, request).await;
        let Output::Stream(stream) = output else { unreachable!() };
        let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
        let expected = "\
+---------------------+---+---+
| ts                  | a | b |
+---------------------+---+---+
| 2023-01-01T07:26:16 |   |   |
| 2023-01-01T07:26:17 | 6 |   |
| 2023-01-01T07:26:18 |   | x |
| 2023-01-01T07:26:20 |   | z |
+---------------------+---+---+";
        assert_eq!(recordbatches.pretty_print().unwrap(), expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_promql_query() {
        common_telemetry::init_default_ut_logging();

        let standalone = tests::create_standalone_instance("test_standalone_promql_query").await;
        let instance = &standalone.instance;

        let table_name = "my_table";
        let sql = format!("CREATE TABLE {table_name} (h string, a double, ts TIMESTAMP, TIME INDEX (ts), PRIMARY KEY(h))");
        create_table(instance, sql).await;

        let insert = InsertRequest {
            table_name: table_name.to_string(),
            columns: vec![
                Column {
                    column_name: "h".to_string(),
                    values: Some(Values {
                        string_values: vec![
                            "t".to_string(),
                            "t".to_string(),
                            "t".to_string(),
                            "t".to_string(),
                            "t".to_string(),
                            "t".to_string(),
                            "t".to_string(),
                            "t".to_string(),
                        ],
                        ..Default::default()
                    }),
                    semantic_type: SemanticType::Tag as i32,
                    datatype: ColumnDataType::String as i32,
                    ..Default::default()
                },
                Column {
                    column_name: "a".to_string(),
                    values: Some(Values {
                        f64_values: vec![1f64, 11f64, 20f64, 22f64, 50f64, 55f64, 99f64],
                        ..Default::default()
                    }),
                    null_mask: vec![4],
                    semantic_type: SemanticType::Field as i32,
                    datatype: ColumnDataType::Float64 as i32,
                },
                Column {
                    column_name: "ts".to_string(),
                    values: Some(Values {
                        ts_millisecond_values: vec![
                            1672557972000,
                            1672557973000,
                            1672557974000,
                            1672557975000,
                            1672557976000,
                            1672557977000,
                            1672557978000,
                            1672557979000,
                        ],
                        ..Default::default()
                    }),
                    semantic_type: SemanticType::Timestamp as i32,
                    datatype: ColumnDataType::TimestampMillisecond as i32,
                    ..Default::default()
                },
            ],
            row_count: 8,
            ..Default::default()
        };

        let request = Request::Insert(insert);
        let output = query(instance, request).await;
        assert!(matches!(output, Output::AffectedRows(8)));

        let request = Request::Query(QueryRequest {
            query: Some(Query::PromRangeQuery(api::v1::PromRangeQuery {
                query: "my_table".to_owned(),
                start: "1672557973".to_owned(),
                end: "1672557978".to_owned(),
                step: "1s".to_owned(),
            })),
        });
        let output = query(instance, request).await;
        let Output::Stream(stream) = output else { unreachable!() };
        let recordbatches = RecordBatches::try_collect(stream).await.unwrap();
        let expected = "\
+---+------+---------------------+
| h | a    | ts                  |
+---+------+---------------------+
| t | 11.0 | 2023-01-01T07:26:13 |
| t |      | 2023-01-01T07:26:14 |
| t | 20.0 | 2023-01-01T07:26:15 |
| t | 22.0 | 2023-01-01T07:26:16 |
| t | 50.0 | 2023-01-01T07:26:17 |
| t | 55.0 | 2023-01-01T07:26:18 |
+---+------+---------------------+";
        assert_eq!(recordbatches.pretty_print().unwrap(), expected);
    }
}