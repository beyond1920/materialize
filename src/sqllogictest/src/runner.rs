// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! The Materialize-specific runner for sqllogictest.
//!
//! slt tests expect a serialized execution of sql statements and queries.
//! To get the same results in materialize we track current_timestamp and increment it whenever we execute a statement.
//!
//! The high-level workflow is:
//!   for each record in the test file:
//!     if record is a sql statement:
//!       run sql in postgres, observe changes and copy them to materialize using LocalInput::Updates(..)
//!       advance current_timestamp
//!       promise to never send updates for times < current_timestamp using LocalInput::Watermark(..)
//!       compare to expected results
//!       if wrong, bail out and stop processing this file
//!     if record is a sql query:
//!       peek query at current_timestamp
//!       compare to expected results
//!       if wrong, record the error

use std::borrow::ToOwned;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops;
use std::path::Path;
use std::str;
use std::time::Duration;

use anyhow::{anyhow, bail};
use bytes::BytesMut;
use itertools::izip;
use lazy_static::lazy_static;
use md5::{Digest, Md5};
use regex::Regex;

use build_info::DUMMY_BUILD_INFO;
use coord::session::Session;
use coord::{ExecuteResponse, TimestampConfig};
use dataflow_types::PeekResponse;
use ore::option::OptionExt;
use ore::thread::{JoinHandleExt, JoinOnDropHandle};
use repr::{ColumnName, ColumnType, RelationDesc, Row, ScalarType};
use sql::ast::Statement;

use crate::ast::{Location, Mode, Output, QueryOutput, Record, Sort, Type};
use crate::util;

#[derive(Debug)]
pub enum Outcome<'a> {
    Unsupported {
        error: anyhow::Error,
        location: Location,
    },
    ParseFailure {
        error: anyhow::Error,
        location: Location,
    },
    PlanFailure {
        error: anyhow::Error,
        location: Location,
    },
    UnexpectedPlanSuccess {
        expected_error: &'a str,
        location: Location,
    },
    WrongNumberOfRowsInserted {
        expected_count: usize,
        actual_count: usize,
        location: Location,
    },
    InferenceFailure {
        expected_types: &'a [Type],
        inferred_types: Vec<ColumnType>,
        message: String,
        location: Location,
    },
    WrongColumnNames {
        expected_column_names: &'a Vec<ColumnName>,
        inferred_column_names: Vec<ColumnName>,
        location: Location,
    },
    OutputFailure {
        expected_output: &'a Output,
        actual_raw_output: Vec<Row>,
        actual_output: Output,
        location: Location,
    },
    Bail {
        cause: Box<Outcome<'a>>,
        location: Location,
    },
    Success,
}

const NUM_OUTCOMES: usize = 10;

impl<'a> Outcome<'a> {
    fn code(&self) -> usize {
        match self {
            Outcome::Unsupported { .. } => 0,
            Outcome::ParseFailure { .. } => 1,
            Outcome::PlanFailure { .. } => 2,
            Outcome::UnexpectedPlanSuccess { .. } => 3,
            Outcome::WrongNumberOfRowsInserted { .. } => 4,
            Outcome::InferenceFailure { .. } => 5,
            Outcome::WrongColumnNames { .. } => 6,
            Outcome::OutputFailure { .. } => 7,
            Outcome::Bail { .. } => 8,
            Outcome::Success => 9,
        }
    }

    fn success(&self) -> bool {
        matches!(self, Outcome::Success)
    }
}

impl fmt::Display for Outcome<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use Outcome::*;
        const INDENT: &str = "\n        ";
        match self {
            Unsupported { error, location } => write!(f, "Unsupported:{}:\n{:#}", location, error),
            ParseFailure { error, location } => {
                write!(f, "ParseFailure:{}:\n{:#}", location, error)
            }
            PlanFailure { error, location } => write!(f, "PlanFailure:{}:\n{:#}", location, error),
            UnexpectedPlanSuccess {
                expected_error,
                location,
            } => write!(
                f,
                "UnexpectedPlanSuccess:{} expected error: {}",
                location, expected_error
            ),
            WrongNumberOfRowsInserted {
                expected_count,
                actual_count,
                location,
            } => write!(
                f,
                "WrongNumberOfRowsInserted:{}{}expected: {}{}actually: {}",
                location, INDENT, expected_count, INDENT, actual_count
            ),
            InferenceFailure {
                expected_types,
                inferred_types,
                message,
                location,
            } => write!(
                f,
                "Inference Failure:{}{}\
                 expected types: {}{}\
                 inferred types: {}{}\
                 message: {}",
                location,
                INDENT,
                expected_types
                    .iter()
                    .map(|s| format!("{:?}", s))
                    .collect::<Vec<_>>()
                    .join(" "),
                INDENT,
                inferred_types
                    .iter()
                    .map(|s| format!("{}", s.scalar_type))
                    .collect::<Vec<_>>()
                    .join(" "),
                INDENT,
                message
            ),
            WrongColumnNames {
                expected_column_names,
                inferred_column_names,
                location,
            } => write!(
                f,
                "Wrong Column Names:{}:{}expected column names: {}{}inferred column names: {}",
                location,
                INDENT,
                expected_column_names
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
                INDENT,
                inferred_column_names
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            OutputFailure {
                expected_output,
                actual_raw_output,
                actual_output,
                location,
            } => write!(
                f,
                "OutputFailure:{}{}expected: {:?}{}actually: {:?}{}actual raw: {:?}",
                location, INDENT, expected_output, INDENT, actual_output, INDENT, actual_raw_output
            ),
            Bail { cause, location } => write!(f, "Bail:{} {}", location, cause),
            Success => f.write_str("Success"),
        }
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct Outcomes([usize; NUM_OUTCOMES]);

impl ops::AddAssign<Outcomes> for Outcomes {
    fn add_assign(&mut self, rhs: Outcomes) {
        for (lhs, rhs) in self.0.iter_mut().zip(rhs.0.iter()) {
            *lhs += rhs
        }
    }
}

impl fmt::Display for Outcomes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let total: usize = self.0.iter().sum();
        write!(f, "{}:", if self.0[9] == total { "PASS" } else { "FAIL" })?;
        lazy_static! {
            static ref NAMES: Vec<&'static str> = vec![
                "unsupported",
                "parse-failure",
                "plan-failure",
                "unexpected-plan-success",
                "wrong-number-of-rows-inserted",
                "inference-failure",
                "wrong-column-names",
                "output-failure",
                "bail",
                "success",
                "total",
            ];
        }
        for (i, n) in self.0.iter().enumerate() {
            if *n > 0 {
                write!(f, " {}={}", NAMES[i], n)?;
            }
        }
        write!(f, " total={}", total)
    }
}

impl Outcomes {
    pub fn any_failed(&self) -> bool {
        self.0[9] < self.0.iter().sum::<usize>()
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "unsupported": self.0[0],
            "parse_failure": self.0[1],
            "plan_failure": self.0[2],
            "unexpected_plan_success": self.0[3],
            "wrong_number_of_rows_affected": self.0[4],
            "inference_failure": self.0[5],
            "wrong_column_names": self.0[6],
            "output_failure": self.0[7],
            "bail": self.0[8],
            "success": self.0[9],
        })
    }
}

const NUM_TIMELY_WORKERS: usize = 3;

pub(crate) struct State {
    // Drop order matters for these fields.
    coord_client: coord::SessionClient,
    _dataflow_workers: Box<dyn Drop>,
    _coord_thread: JoinOnDropHandle<()>,
}

fn format_row(
    row: &Row,
    col_types: &[ColumnType],
    slt_types: &[Type],
    mode: Mode,
    sort: &Sort,
) -> Vec<String> {
    let row = izip!(slt_types, col_types, row.iter()).map(|(slt_typ, col_typ, d)| {
        if d.is_null() {
            return "NULL".to_owned();
        }

        match (slt_typ, &col_typ.scalar_type) {
            (Type::Bool, ScalarType::Bool) | (Type::Text, ScalarType::Bool) => {
                d.unwrap_bool().to_string()
            }

            (Type::Integer, ScalarType::Int32) => d.unwrap_int32().to_string(),
            (Type::Integer, ScalarType::Int64) => d.unwrap_int64().to_string(),
            (Type::Integer, ScalarType::Decimal(_, s)) => {
                let d = d.unwrap_decimal().with_scale(*s);
                format!("{:.0}", d)
            }
            (Type::Integer, ScalarType::Float32) => format!("{:.0}", d.unwrap_float32().trunc()),
            (Type::Integer, ScalarType::Float64) => format!("{:.0}", d.unwrap_float64().trunc()),
            (Type::Integer, ScalarType::String) => "0".to_owned(),
            (Type::Integer, ScalarType::Bool) => i8::from(d.unwrap_bool()).to_string(),

            (Type::Real, ScalarType::Int32) => format!("{:.3}", d.unwrap_int32()),
            (Type::Real, ScalarType::Int64) => format!("{:.3}", d.unwrap_int64()),
            (Type::Real, ScalarType::Float32) => match mode {
                Mode::Standard => format!("{:.3}", d.unwrap_float32()),
                Mode::Cockroach => format!("{}", d.unwrap_float32()),
            },
            (Type::Real, ScalarType::Float64) => match mode {
                Mode::Standard => format!("{:.3}", d.unwrap_float64()),
                Mode::Cockroach => format!("{}", d.unwrap_float64()),
            },
            (Type::Real, ScalarType::Decimal(_, s)) => {
                let d = d.unwrap_decimal().with_scale(*s);
                match mode {
                    Mode::Standard => format!("{:.3}", d),
                    Mode::Cockroach => format!("{}", d),
                }
            }

            (Type::Text, ScalarType::Int32) => format!("{}", d.unwrap_int32()),
            (Type::Text, ScalarType::Int64) => format!("{}", d.unwrap_int64()),
            (Type::Text, ScalarType::Float32) => format!("{:.3}", d.unwrap_float32()),
            (Type::Text, ScalarType::Float64) => format!("{:.3}", d.unwrap_float64()),
            // Bytes are printed as text iff they are valid UTF-8. This
            // seems guaranteed to confuse everyone, but it is required for
            // compliance with the CockroachDB sqllogictest runner. [0]
            //
            // [0]: https://github.com/cockroachdb/cockroach/blob/970782487/pkg/sql/logictest/logic.go#L2038-L2043
            (Type::Text, ScalarType::Bytes) => match str::from_utf8(d.unwrap_bytes()) {
                Ok(s) => s.to_owned(),
                Err(_) => format!("{:?}", d.unwrap_bytes()),
            },
            (Type::Text, ScalarType::String) => match d.unwrap_str() {
                "" => "(empty)".to_owned(),
                s => s.to_owned(),
            },
            (Type::Text, _) => {
                let mut buf = BytesMut::new();
                pgrepr::Value::from_datum(d, &col_typ.scalar_type)
                    .unwrap()
                    .encode_text(&mut buf);
                str::from_utf8(&buf).unwrap().to_owned()
            }

            (Type::Oid, ScalarType::Oid) => format!("{}", d.unwrap_int32()),

            other => panic!("Don't know how to format {:?}", other),
        }
    });
    if mode == Mode::Cockroach && sort.yes() {
        row.flat_map(|s| {
            crate::parser::split_cols(&s, slt_types.len())
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
    } else {
        row.collect()
    }
}

impl State {
    pub async fn start() -> Result<Self, anyhow::Error> {
        let process_id = 0;

        let switchboard = comm::Switchboard::local()?;

        let (cmd_tx, cmd_rx) = futures::channel::mpsc::unbounded();
        let dataflow_workers = dataflow::serve(
            vec![None],
            NUM_TIMELY_WORKERS,
            process_id,
            switchboard.clone(),
        )
        .unwrap();

        // Note that the coordinator must be initialized *after* launching the
        // dataflow workers, as booting the coordinator can involve sending enough
        // data to workers to fill up a `comm` channel buffer (#3280).
        let (coord_thread, _cluster_id) = coord::serve(coord::Config {
            switchboard,
            cmd_rx,
            num_timely_workers: NUM_TIMELY_WORKERS,
            symbiosis_url: Some("postgres://"),
            data_directory: None,
            logging: None,
            timestamp: TimestampConfig {
                frequency: Duration::from_millis(10),
            },
            cache: None,
            logical_compaction_window: None,
            experimental_mode: true,
            build_info: &DUMMY_BUILD_INFO,
        })
        .await?;
        let coord_thread = coord_thread.join_on_drop();

        Ok(State {
            coord_client: coord::Client::new(cmd_tx).for_session(Session::dummy()),
            _dataflow_workers: Box::new(dataflow_workers),
            _coord_thread: coord_thread,
        })
    }

    async fn run_record<'a>(
        &mut self,
        record: &'a Record<'a>,
    ) -> Result<Outcome<'a>, anyhow::Error> {
        match &record {
            Record::Statement {
                expected_error,
                rows_affected,
                sql,
                location,
            } => {
                match self
                    .run_statement(*expected_error, *rows_affected, sql, location.clone())
                    .await?
                {
                    Outcome::Success => Ok(Outcome::Success),
                    other => {
                        if expected_error.is_some() {
                            Ok(other)
                        } else {
                            // If we failed to execute a statement that was supposed to succeed,
                            // running the rest of the tests in this file will probably cause
                            // false positives, so just give up on the file entirely.
                            Ok(Outcome::Bail {
                                cause: Box::new(other),
                                location: location.clone(),
                            })
                        }
                    }
                }
            }
            Record::Query {
                sql,
                output,
                location,
            } => self.run_query(sql, output, location.clone()).await,
            _ => Ok(Outcome::Success),
        }
    }

    async fn run_statement<'a>(
        &mut self,
        expected_error: Option<&'a str>,
        expected_rows_affected: Option<usize>,
        sql: &'a str,
        location: Location,
    ) -> Result<Outcome<'a>, anyhow::Error> {
        lazy_static! {
            static ref UNSUPPORTED_INDEX_STATEMENT_REGEX: Regex =
                Regex::new("^(CREATE UNIQUE INDEX|REINDEX)").unwrap();
        }
        if UNSUPPORTED_INDEX_STATEMENT_REGEX.is_match(sql) {
            // sure, we totally made you an index
            return Ok(Outcome::Success);
        }

        match self.run_sql(sql).await {
            Ok((_desc, resp)) => {
                if let Some(expected_error) = expected_error {
                    return Ok(Outcome::UnexpectedPlanSuccess {
                        expected_error,
                        location,
                    });
                }
                match expected_rows_affected {
                    None => Ok(Outcome::Success),
                    Some(expected) => match resp {
                        ExecuteResponse::Inserted(actual)
                        | ExecuteResponse::Updated(actual)
                        | ExecuteResponse::Deleted(actual) => {
                            if expected != actual {
                                Ok(Outcome::WrongNumberOfRowsInserted {
                                    expected_count: expected,
                                    actual_count: actual,
                                    location,
                                })
                            } else {
                                Ok(Outcome::Success)
                            }
                        }

                        _ => Ok(Outcome::PlanFailure {
                            error: anyhow!("Query did not insert any rows, expected {}", expected,),
                            location,
                        }),
                    },
                }
            }
            Err(error) => {
                if let Some(expected_error) = expected_error {
                    if Regex::new(expected_error)?.is_match(&format!("{:#}", error)) {
                        return Ok(Outcome::Success);
                    }
                }
                Ok(Outcome::PlanFailure { error, location })
            }
        }
    }

    async fn run_query<'a>(
        &mut self,
        sql: &'a str,
        output: &'a Result<QueryOutput<'_>, &'a str>,
        location: Location,
    ) -> Result<Outcome<'a>, anyhow::Error> {
        // get statement
        let statements = match sql::parse::parse(sql) {
            Ok(statements) => statements,
            Err(_) if output.is_err() => return Ok(Outcome::Success),
            Err(e) => {
                return Ok(Outcome::ParseFailure {
                    error: e.into(),
                    location,
                })
            }
        };
        let statement = match &*statements {
            [] => bail!("Got zero statements?"),
            [statement] => statement,
            _ => bail!("Got multiple statements: {:?}", statements),
        };
        match statement {
            Statement::CreateView { .. }
            | Statement::Select { .. }
            | Statement::ShowIndexes { .. } => (),
            _ => {
                if output.is_err() {
                    // We're not interested in testing our hacky handling of INSERT etc
                    return Ok(Outcome::Success);
                }
            }
        }

        // send plan, read response
        let (desc, rows) = match self.run_sql(sql).await {
            Ok((desc, ExecuteResponse::SendingRows(rx))) => {
                let desc = desc.expect("RelationDesc missing for query that returns rows");
                let rows = match rx.await? {
                    PeekResponse::Rows(rows) => Ok(rows),
                    PeekResponse::Error(e) => Err(anyhow!("{}", e)),
                    PeekResponse::Canceled => {
                        panic!("sqllogictest query cannot possibly be canceled")
                    }
                };
                (desc, rows)
            }
            Ok(other) => {
                return Ok(Outcome::PlanFailure {
                    error: anyhow!(
                        "Query did not result in SendingRows, instead got {:?}",
                        other
                    ),
                    location,
                });
            }
            Err(e) => (RelationDesc::empty(), Err(e)),
        };

        let raw_output = match rows {
            Ok(rows) => rows,
            Err(error) => {
                return match output {
                    Ok(_) => {
                        let error_string = format!("{}", error);
                        if error_string.contains("supported") || error_string.contains("overload") {
                            // this is a failure, but it's caused by lack of support rather than by bugs
                            Ok(Outcome::Unsupported { error, location })
                        } else {
                            Ok(Outcome::PlanFailure { error, location })
                        }
                    }
                    Err(expected_error) => {
                        if Regex::new(expected_error)?.is_match(&format!("{:#}", error)) {
                            Ok(Outcome::Success)
                        } else {
                            Ok(Outcome::PlanFailure { error, location })
                        }
                    }
                };
            }
        };

        // unpack expected output
        let QueryOutput {
            sort,
            types: expected_types,
            column_names: expected_column_names,
            output: expected_output,
            mode,
            ..
        } = match output {
            Err(expected_error) => {
                return Ok(Outcome::UnexpectedPlanSuccess {
                    expected_error,
                    location,
                });
            }
            Ok(query_output) => query_output,
        };

        // check that inferred types match expected types
        let inferred_types = &desc.typ().column_types;
        // sqllogictest coerces the output into the expected type, so `expected_types` is often wrong :(
        // but at least it will be the correct length
        if inferred_types.len() != expected_types.len() {
            return Ok(Outcome::InferenceFailure {
                expected_types,
                inferred_types: inferred_types.to_vec(),
                message: format!(
                    "Expected {} types, got {} types",
                    expected_types.len(),
                    inferred_types.len()
                ),
                location,
            });
        }

        // check that output matches inferred types
        for row in &raw_output {
            if row.unpack().len() != inferred_types.len() {
                return Ok(Outcome::InferenceFailure {
                    expected_types,
                    inferred_types: inferred_types.to_vec(),
                    message: format!(
                        "Expected {} datums, got {} datums in row {:?}",
                        expected_types.len(),
                        inferred_types.len(),
                        row
                    ),
                    location,
                });
            }
            for (inferred_type, datum) in inferred_types.iter().zip(row.iter()) {
                if !datum.is_instance_of(inferred_type) {
                    return Ok(Outcome::InferenceFailure {
                        expected_types,
                        inferred_types: inferred_types.to_vec(),
                        message: format!(
                            "Inferred type {:?}, got datum {:?}",
                            inferred_type, datum,
                        ),
                        location,
                    });
                }
            }
        }

        // check column names
        if let Some(expected_column_names) = expected_column_names {
            let inferred_column_names = desc
                .iter_names()
                .map(|t| t.owned().unwrap_or_else(|| "?column?".into()))
                .collect::<Vec<_>>();
            if expected_column_names != &inferred_column_names {
                return Ok(Outcome::WrongColumnNames {
                    expected_column_names,
                    inferred_column_names,
                    location,
                });
            }
        }

        // format output
        let mut formatted_rows = raw_output
            .iter()
            .map(|row| format_row(&row, inferred_types, &**expected_types, *mode, sort))
            .collect::<Vec<_>>();

        // sort formatted output
        if let Sort::Row = sort {
            formatted_rows.sort();
        }
        let mut values = formatted_rows.into_iter().flatten().collect::<Vec<_>>();
        if let Sort::Value = sort {
            values.sort();
        }

        // check output
        match expected_output {
            Output::Values(expected_values) => {
                if values != *expected_values {
                    return Ok(Outcome::OutputFailure {
                        expected_output,
                        actual_raw_output: raw_output,
                        actual_output: Output::Values(values),
                        location,
                    });
                }
            }
            Output::Hashed {
                num_values,
                md5: expected_md5,
            } => {
                let mut hasher = Md5::new();
                for value in &values {
                    hasher.update(value);
                    hasher.update("\n");
                }
                let md5 = format!("{:x}", hasher.finalize());
                if values.len() != *num_values || md5 != *expected_md5 {
                    return Ok(Outcome::OutputFailure {
                        expected_output,
                        actual_raw_output: raw_output,
                        actual_output: Output::Hashed {
                            num_values: values.len(),
                            md5,
                        },
                        location,
                    });
                }
            }
        }

        Ok(Outcome::Success)
    }

    pub(crate) async fn run_sql(
        &mut self,
        sql: &str,
    ) -> Result<(Option<RelationDesc>, ExecuteResponse), anyhow::Error> {
        let stmts = sql::parse::parse(sql)?;
        let stmt = if stmts.len() == 1 {
            stmts.into_iter().next().unwrap()
        } else {
            bail!("Expected exactly one statement, got: {}", sql);
        };
        let statement_name = String::from("");
        let portal_name = String::from("");

        // Parse.
        self.coord_client
            .describe(statement_name.clone(), Some(stmt), vec![])
            .await?;

        // Bind.
        let stmt = self
            .coord_client
            .session()
            .get_prepared_statement(&statement_name)
            .expect("unnamed prepared statement missing");
        let desc = stmt.desc().clone();
        let stmt = stmt.sql().cloned();
        let result_formats = vec![pgrepr::Format::Text; desc.arity()];
        self.coord_client.session().set_portal(
            portal_name.clone(),
            desc.clone(),
            stmt,
            vec![],
            result_formats,
        )?;

        // Execute.
        let res = self.coord_client.execute(portal_name).await?;
        Ok((desc.relation_desc, res))
    }
}

fn print_record(record: &Record) {
    match record {
        Record::Statement { sql, .. } | Record::Query { sql, .. } => {
            println!("{}", crate::util::indent(sql, 4))
        }
        _ => (),
    }
}

pub async fn run_string(
    source: &str,
    input: &str,
    verbosity: usize,
) -> Result<Outcomes, anyhow::Error> {
    let mut outcomes = Outcomes::default();
    let mut state = State::start().await.unwrap();
    let mut parser = crate::parser::Parser::new(source, input);
    println!("==> {}", source);
    for record in parser.parse_records()? {
        // In maximal-verbosity mode, print the query before attempting to run
        // it. Running the query might panic, so it is important to print out
        // what query we are trying to run *before* we panic.
        if verbosity >= 2 {
            print_record(&record);
        }

        let outcome = state
            .run_record(&record)
            .await
            .map_err(|err| format!("In {}:\n{}", source, err))
            .unwrap();

        // Print failures in verbose mode.
        if verbosity >= 1 && !outcome.success() {
            if verbosity < 2 {
                // If `verbosity >= 2`, we'll already have printed the record,
                // so don't print it again. Yes, this is an ugly bit of logic.
                // Please don't try to consolidate it with the `print_record`
                // call above, as it's important to have a mode in which records
                // are printed before they are run, so that if running the
                // record panics, you can tell which record caused it.
                print_record(&record);
            }
            println!("{}", util::indent(&outcome.to_string(), 4));
            println!("{}", util::indent("----", 4));
        }

        outcomes.0[outcome.code()] += 1;

        if let Outcome::Bail { .. } = outcome {
            break;
        }
    }
    Ok(outcomes)
}

pub async fn run_file(filename: &Path, verbosity: usize) -> Result<Outcomes, anyhow::Error> {
    let mut input = String::new();
    File::open(filename)?.read_to_string(&mut input)?;
    run_string(&format!("{}", filename.display()), &input, verbosity).await
}

pub async fn run_stdin(verbosity: usize) -> Result<Outcomes, anyhow::Error> {
    let mut input = String::new();
    std::io::stdin().lock().read_to_string(&mut input)?;
    run_string("<stdin>", &input, verbosity).await
}

pub async fn rewrite_file(filename: &Path, _verbosity: usize) -> Result<(), anyhow::Error> {
    let mut file = OpenOptions::new().read(true).write(true).open(filename)?;

    let mut input = String::new();
    file.read_to_string(&mut input)?;

    let mut buf = RewriteBuffer::new(&input);

    let mut state = State::start().await?;
    let mut parser = crate::parser::Parser::new(filename.to_str().unwrap_or(""), &input);
    println!("==> {}", filename.display());
    for record in parser.parse_records()? {
        let record = record;
        let outcome = state.run_record(&record).await?;

        // If we see an output failure for a query, rewrite the expected output
        // to match the observed output.
        if let (
            Record::Query {
                output:
                    Ok(QueryOutput {
                        mode,
                        output: Output::Values(_),
                        output_str: expected_output,
                        types,
                        ..
                    }),
                ..
            },
            Outcome::OutputFailure {
                actual_output: Output::Values(actual_output),
                ..
            },
        ) = (&record, &outcome)
        {
            // Output everything before this record.
            let offset = expected_output.as_ptr() as usize - input.as_ptr() as usize;
            buf.flush_to(offset);
            buf.skip_to(offset + expected_output.len());

            // Attempt to install the result separator (----), if it does
            // not already exist.
            if buf.peek_last(5) == "\n----" {
                buf.append("\n");
            } else if buf.peek_last(6) != "\n----\n" {
                buf.append("\n----\n");
            }

            for (i, row) in actual_output.chunks(types.len()).enumerate() {
                match mode {
                    // In Cockroach mode, output each row on its own line, with
                    // two spaces between each column.
                    Mode::Cockroach => {
                        if i != 0 {
                            buf.append("\n");
                        }
                        buf.append(&row.join("  "));
                    }
                    // In standard mode, output each value on its own line,
                    // and ignore row boundaries.
                    Mode::Standard => {
                        for (j, col) in row.iter().enumerate() {
                            if i != 0 || j != 0 {
                                buf.append("\n");
                            }
                            buf.append(col);
                        }
                    }
                }
            }
        } else if let Outcome::Success = outcome {
            // Ok.
        } else {
            bail!("unexpected: {}", outcome);
        }
    }

    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(buf.finish().as_bytes())?;
    file.sync_all()?;
    Ok(())
}

struct RewriteBuffer<'a> {
    input: &'a str,
    input_offset: usize,
    output: String,
}

impl<'a> RewriteBuffer<'a> {
    fn new(input: &'a str) -> RewriteBuffer<'a> {
        RewriteBuffer {
            input,
            input_offset: 0,
            output: String::new(),
        }
    }

    fn flush_to(&mut self, offset: usize) {
        assert!(offset >= self.input_offset);
        let chunk = &self.input[self.input_offset..offset];
        self.output.push_str(chunk);
        self.input_offset = offset;
    }

    fn skip_to(&mut self, offset: usize) {
        assert!(offset >= self.input_offset);
        self.input_offset = offset;
    }

    fn append(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn peek_last(&self, n: usize) -> &str {
        &self.output[self.output.len() - n..]
    }

    fn finish(mut self) -> String {
        self.flush_to(self.input.len());
        self.output
    }
}
