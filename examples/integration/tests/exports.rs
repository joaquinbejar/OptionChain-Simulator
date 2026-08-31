//! The export matrix: every dataset in every format the deployment ADVERTISES.
//!
//! The export is what a backtester consumes and it has the widest surface in
//! the service: three datasets, four encodings, a range and three greek
//! levels. It also streams, so its failure modes are the ones that only appear
//! over a socket.
//!
//! Which formats are exercised comes from the deployment's own OpenAPI
//! document rather than from probing and shrugging: a format the document
//! advertises must work, or refuse with the typed error that says this build
//! was compiled without it. Anything else is a regression, not a skip.
//!
//! Exports are kept small, three steps over a two-strike chain, because these
//! run against a shared deployment and an export is the most expensive thing
//! it does.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Response, ServiceClient, packed, reference_request, service};

/// Steps in every exported tape.
const STEPS: usize = 3;

/// The datasets the export offers.
const DATASETS: [&str; 3] = ["underlying", "volatility", "option_chains"];

/// A simulation walked to the end, so its tape is complete, that deletes
/// itself afterwards.
struct Exported {
    client: ServiceClient,
    id: String,
}

impl Exported {
    /// Creates a simulation and walks it to exhaustion.
    fn create(client: &ServiceClient) -> Option<Self> {
        let mut request = reference_request("SPX");
        if let Some(object) = request.as_object_mut() {
            object.insert("steps".to_string(), serde_json::json!(STEPS));
            object.insert("chain_size".to_string(), serde_json::json!(2));
        }

        let response = match client.post("/api/v2/simulations", &request) {
            Ok(response) => response,
            Err(error) => panic!("{error}"),
        };
        if response.status == 404 {
            println!("SKIP: this deployment does not serve /api/v2/simulations");
            return None;
        }
        assert_eq!(
            response.status,
            201,
            "creating a simulation must answer 201, got {}",
            response.text()
        );

        let body: serde_json::Value = match response.json("/api/v2/simulations") {
            Ok(body) => body,
            Err(error) => panic!("{error}"),
        };
        let id = match body.get("id").and_then(serde_json::Value::as_str) {
            Some(id) => id.to_string(),
            None => panic!("a created simulation must carry an id: {body}"),
        };

        let exported = Self {
            client: client.clone(),
            id,
        };
        for step in 0..STEPS {
            let path = format!("/api/v2/simulations/{}/step", exported.id);
            match client.request("POST", &path, None) {
                Ok(response) => assert_eq!(
                    response.status,
                    200,
                    "step {step} must serve before the tape can be exported: {}",
                    response.text()
                ),
                Err(error) => panic!("{error}"),
            }
        }
        Some(exported)
    }

    /// One export, with whatever query the case needs.
    fn export(&self, query: &str) -> Response {
        let path = format!("/api/v2/simulations/{}/export?{query}", self.id);
        match self.client.get(&path) {
            Ok(response) => response,
            Err(error) => panic!("exporting {query}: {error}"),
        }
    }
}

impl Drop for Exported {
    fn drop(&mut self) {
        let path = format!("/api/v2/simulations/{}", self.id);
        examples_integration::report_cleanup(&self.client, &path, &self.id);
    }
}

/// The formats the deployment's OpenAPI document advertises, and for each,
/// whether this build actually serves it.
struct Offered {
    /// Advertised and serving.
    usable: Vec<String>,
    /// Advertised but compiled out, with the reason the service gave.
    absent: Vec<(String, String)>,
}

/// Reads the advertised formats from the deployment's own document, then
/// establishes which of them this build serves.
///
/// A format the document advertises may only be missing for one reason: the
/// build was compiled without it, which the service says in a typed 400 naming
/// `format`. Any other refusal is a regression, and a silent skip would hide
/// it.
fn offered_formats(client: &ServiceClient, exported: &Exported) -> Offered {
    let document: serde_json::Value = match client.get("/api-docs/openapi.json") {
        Ok(response) if response.status == 200 => match response.json("/api-docs/openapi.json") {
            Ok(document) => document,
            Err(error) => panic!("{error}"),
        },
        Ok(response) => panic!(
            "the deployment must serve an OpenAPI document to drive this test, got {}",
            response.status
        ),
        Err(error) => panic!("{error}"),
    };

    let description = document
        .pointer("/paths/~1api~1v2~1simulations~1{id}~1export/get/parameters")
        .and_then(serde_json::Value::as_array)
        .and_then(|parameters| {
            parameters.iter().find(|parameter| {
                parameter.get("name").and_then(|name| name.as_str()) == Some("format")
            })
        })
        .and_then(|parameter| parameter.get("description"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("the document must describe the export format parameter"));

    // The description opens with the alternatives, "json | csv | arrow |
    // packed", which is the contract this suite has to hold the deployment to.
    let advertised: Vec<String> = description
        .split('.')
        .next()
        .unwrap_or_default()
        .split('|')
        .map(|format| format.trim().to_string())
        .filter(|format| !format.is_empty() && format.chars().all(|c| c.is_ascii_lowercase()))
        .collect();
    assert!(
        advertised.contains(&"json".to_string()) && advertised.contains(&"csv".to_string()),
        "the document must advertise at least json and csv, it advertises {advertised:?}"
    );

    let mut usable = Vec::new();
    let mut absent = Vec::new();
    for format in advertised {
        let response = exported.export(&format!("dataset=underlying&format={format}"));
        match response.status {
            200 => usable.push(format),
            400 => {
                let body: serde_json::Value = match response.json("/export") {
                    Ok(body) => body,
                    Err(error) => panic!("a refusal must be the documented shape: {error}"),
                };
                let message = body
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                assert_eq!(
                    body.get("field").and_then(serde_json::Value::as_str),
                    Some("format"),
                    "a refused format must name the format field: {body}"
                );
                assert!(
                    message.contains("unavailable") || message.contains("feature"),
                    "{format} is advertised, so the only acceptable refusal is one saying this \
                     build was compiled without it; the service said {message:?}"
                );
                absent.push((format, message));
            }
            other => panic!(
                "{format} is advertised and answered {other}: {}",
                response.text()
            ),
        }
    }

    for (format, reason) in &absent {
        println!("INFO: {format} is advertised but not built here: {reason}");
    }
    Offered { usable, absent }
}

/// Parses an RFC 4180 document: CRLF-separated records, commas between
/// fields, double quotes around a field that contains either, and a doubled
/// quote for a literal one.
fn parse_csv(body: &str, what: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = body.chars().peekable();

    while let Some(character) = characters.next() {
        match (quoted, character) {
            (true, '"') => {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            (true, other) => field.push(other),
            (false, '"') => quoted = true,
            (false, ',') => record.push(std::mem::take(&mut field)),
            (false, '\r') => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            (false, '\n') => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            (false, other) => field.push(other),
        }
    }
    assert!(!quoted, "{what}: the csv ends inside a quoted field");
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

/// The header and the data rows of a CSV export, with every row required to
/// carry exactly as many fields as the header declares.
fn csv_table(body: &str, what: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let records = parse_csv(body, what);
    let (header, rows) = match records.split_first() {
        Some((header, rows)) => (header.clone(), rows.to_vec()),
        None => panic!("{what}: a csv export must carry a header"),
    };
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            row.len(),
            header.len(),
            "{what}: row {index} carries {} fields where the header declares {}: {row:?}",
            row.len(),
            header.len()
        );
    }
    (header, rows)
}

/// Two rendered values are the same value.
///
/// Compared numerically when both parse as numbers, because `5000` and
/// `5000.0` are the same price rendered by two encodings, and textually
/// otherwise.
fn same_value(left: &str, right: &str) -> bool {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => (left - right).abs() <= 1e-9 * left.abs().max(1.0),
        _ => left == right,
    }
}

/// Compares a decoded table with the CSV one, column by column and row by row.
fn assert_same_table(
    what: &str,
    csv_header: &[String],
    csv_rows: &[Vec<String>],
    header: &[String],
    rows: &[Vec<String>],
) {
    assert_eq!(
        header, csv_header,
        "{what}: the columns differ from the csv ones"
    );
    assert_eq!(
        rows.len(),
        csv_rows.len(),
        "{what}: {} rows against {} as csv",
        rows.len(),
        csv_rows.len()
    );
    for (index, (row, csv_row)) in rows.iter().zip(csv_rows).enumerate() {
        assert_eq!(
            row.len(),
            csv_row.len(),
            "{what}: row {index} has {} values against {} as csv",
            row.len(),
            csv_row.len()
        );
        for (column, (value, csv_value)) in row.iter().zip(csv_row).enumerate() {
            assert!(
                same_value(value, csv_value),
                "{what}: row {index} column {} is {value:?} and {csv_value:?} as csv",
                csv_header.get(column).map_or("?", String::as_str)
            );
        }
    }
}

/// Decodes an Arrow IPC stream into the same shape, when this test was built
/// with the feature that can.
#[cfg(feature = "arrow-export")]
fn arrow_table(bytes: &[u8], what: &str) -> (Vec<String>, Vec<Vec<String>>) {
    use arrow::ipc::reader::StreamReader;

    let reader = match StreamReader::try_new(std::io::Cursor::new(bytes.to_vec()), None) {
        Ok(reader) => reader,
        Err(error) => panic!("{what}: an arrow export must be a readable stream: {error}"),
    };

    let mut header: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in reader {
        let batch = match batch {
            Ok(batch) => batch,
            Err(error) => panic!("{what}: an arrow batch must decode: {error}"),
        };
        if header.is_empty() {
            header = batch
                .schema()
                .fields()
                .iter()
                .map(|field| field.name().to_string())
                .collect();
        }
        for index in 0..batch.num_rows() {
            let mut row = Vec::with_capacity(batch.num_columns());
            for column in batch.columns() {
                row.push(arrow_cell(column.as_ref(), index));
            }
            rows.push(row);
        }
    }
    (header, rows)
}

/// Renders one Arrow cell the way the text encodings render it.
#[cfg(feature = "arrow-export")]
fn arrow_cell(column: &dyn arrow::array::Array, index: usize) -> String {
    use arrow::array::{Float64Array, Int64Array, StringArray, TimestampNanosecondArray};
    use arrow::datatypes::DataType;

    if column.is_null(index) {
        return String::new();
    }
    match column.data_type() {
        DataType::Float64 => column
            .as_any()
            .downcast_ref::<Float64Array>()
            .map(|values| values.value(index).to_string())
            .unwrap_or_default(),
        DataType::Int64 => column
            .as_any()
            .downcast_ref::<Int64Array>()
            .map(|values| values.value(index).to_string())
            .unwrap_or_default(),
        DataType::Utf8 => column
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|values| values.value(index).to_string())
            .unwrap_or_default(),
        DataType::Timestamp(_, _) => column
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .and_then(|values| values.value_as_datetime(index))
            .map(|value| format!("{}Z", value.format("%Y-%m-%dT%H:%M:%S")))
            .unwrap_or_default(),
        other => panic!("an arrow export carries an unexpected column type {other}"),
    }
}

/// A deployment serves every format its own document advertises.
///
/// The two can disagree: `arrow-export` is a Cargo feature, so a build without
/// it refuses `format=arrow` in a typed 400 while the OpenAPI document the same
/// process serves still lists the format. That is a contract a client cannot
/// follow, and it shipped in every published image up to 0.2.25 (issue #148).
///
/// The rest of this file treats an advertised-but-absent format as a skip,
/// which is what lets it run anywhere. This test is where that stops being
/// acceptable, because what it holds is not the format matrix but the
/// agreement between the document and the build behind it.
#[test]
fn test_the_deployment_serves_every_format_it_advertises() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let offered = offered_formats(&client, &exported);

    assert!(
        offered.absent.is_empty(),
        "this deployment advertises {:?} and cannot serve them: {:?}. A format in the document \
         is a promise to a client that reads it, so either the build carries the format or the \
         document stops offering it",
        offered
            .absent
            .iter()
            .map(|(format, _)| format.as_str())
            .collect::<Vec<_>>(),
        offered.absent
    );
    println!(
        "INFO: every advertised format is served: {}",
        offered.usable.join(", ")
    );
}

/// Every dataset carries the same rows and the same values in every format the
/// deployment advertises and serves.
#[test]
fn test_every_dataset_agrees_across_every_offered_format() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let offered = offered_formats(&client, &exported);

    for dataset in DATASETS {
        let csv = exported.export(&format!("dataset={dataset}&format=csv"));
        assert_eq!(csv.status, 200, "{dataset} as csv: {}", csv.text());
        let text = csv.text();
        assert!(
            text.contains("\r\n"),
            "{dataset} as csv must use RFC 4180 CRLF endings"
        );
        let (csv_header, csv_rows) = csv_table(&text, dataset);
        assert!(
            !csv_rows.is_empty(),
            "{dataset} must export rows for a walked tape"
        );

        // json: the same keys as the csv header, on every row, and the same
        // values. A row with a missing key fails rather than being skipped.
        let json = exported.export(&format!("dataset={dataset}&format=json"));
        assert_eq!(json.status, 200, "{dataset} as json: {}", json.text());
        let rows: Vec<serde_json::Map<String, serde_json::Value>> =
            match json.json(&format!("{dataset} as json")) {
                Ok(rows) => rows,
                Err(error) => panic!("a json export must be an array of objects: {error}"),
            };
        assert_eq!(
            rows.len(),
            csv_rows.len(),
            "{dataset} exports {} rows as json against {} as csv",
            rows.len(),
            csv_rows.len()
        );
        for (index, (row, csv_row)) in rows.iter().zip(&csv_rows).enumerate() {
            let mut keys: Vec<&String> = row.keys().collect();
            keys.sort();
            let mut expected: Vec<&String> = csv_header.iter().collect();
            expected.sort();
            assert_eq!(
                keys, expected,
                "{dataset} row {index} carries different keys from the csv header"
            );
            for (column, csv_value) in csv_header.iter().zip(csv_row) {
                let value = match row.get(column) {
                    Some(serde_json::Value::Null) => String::new(),
                    Some(serde_json::Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => unreachable!("the keys were compared above"),
                };
                assert!(
                    same_value(&value, csv_value),
                    "{dataset} row {index} column {column} is {value:?} as json and \
                     {csv_value:?} as csv"
                );
            }
        }

        if offered.usable.iter().any(|format| format == "packed") {
            let response = exported.export(&format!("dataset={dataset}&format=packed"));
            assert_eq!(
                response.status,
                200,
                "{dataset} as packed: {}",
                response.text()
            );
            let document = match packed::decode(&response.body) {
                Ok(document) => document,
                Err(error) => panic!("{dataset} as packed must decode: {error}"),
            };
            assert!(
                document.misaligned.is_empty(),
                "{dataset} as packed has payloads at unaligned offsets {:?}, which makes a \
                 zero-copy typed-array view throw",
                document.misaligned
            );
            assert_eq!(
                document.declared_rows as usize,
                csv_rows.len(),
                "{dataset} declares {} rows in its packed footer against {} as csv",
                document.declared_rows,
                csv_rows.len()
            );
            assert_same_table(
                &format!("{dataset} as packed"),
                &csv_header,
                &csv_rows,
                &document.columns,
                &document.rows,
            );
        }

        if offered.usable.iter().any(|format| format == "arrow") {
            let response = exported.export(&format!("dataset={dataset}&format=arrow"));
            assert_eq!(
                response.status,
                200,
                "{dataset} as arrow: {}",
                response.text()
            );
            #[cfg(feature = "arrow-export")]
            {
                let (header, rows) = arrow_table(&response.body, &format!("{dataset} as arrow"));
                assert_same_table(
                    &format!("{dataset} as arrow"),
                    &csv_header,
                    &csv_rows,
                    &header,
                    &rows,
                );
            }
            #[cfg(not(feature = "arrow-export"))]
            println!(
                "INFO: {dataset} as arrow was served but not decoded; build this test with \
                 --features arrow-export to compare its values"
            );
        }
    }
}

/// The same export twice is byte-identical, which is what lets a consumer
/// cache or checksum one.
#[test]
fn test_the_same_export_twice_is_byte_identical() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let offered = offered_formats(&client, &exported);

    for dataset in DATASETS {
        for format in &offered.usable {
            let query = format!("dataset={dataset}&format={format}");
            let first = exported.export(&query);
            let second = exported.export(&query);
            assert_eq!(first.status, 200);
            assert_eq!(second.status, 200);
            assert_eq!(
                first.body, second.body,
                "{dataset} as {format} differs between two identical exports"
            );
        }
    }
}

/// A range selects the same rows in every format, and the two ways to get one
/// wrong are typed rejections naming the field at fault.
#[test]
fn test_a_range_selects_the_same_rows_in_every_format() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let offered = offered_formats(&client, &exported);

    // A valid range, decoded in every offered format and compared against the
    // csv rows it selected.
    let ranged = "dataset=underlying&from_step=1&to_step=2";
    let csv = exported.export(&format!("{ranged}&format=csv"));
    assert_eq!(csv.status, 200, "{}", csv.text());
    let text = csv.text();
    let (csv_header, csv_rows) = csv_table(&text, "a ranged underlying export");
    assert!(
        !csv_rows.is_empty() && csv_rows.len() <= STEPS,
        "a range of two steps must select between one and {STEPS} rows, got {}",
        csv_rows.len()
    );
    // And it selected the steps asked for, not just the right number of them.
    if let Some(step) = csv_header.iter().position(|column| column == "step") {
        let steps: Vec<&str> = csv_rows
            .iter()
            .filter_map(|row| row.get(step).map(String::as_str))
            .collect();
        assert_eq!(
            steps,
            vec!["1", "2"],
            "the range must select exactly the steps it names"
        );
    }

    for format in &offered.usable {
        let response = exported.export(&format!("{ranged}&format={format}"));
        assert_eq!(response.status, 200, "{format}: {}", response.text());
        match format.as_str() {
            "csv" => {}
            "json" => {
                let rows: Vec<serde_json::Value> = match response.json("a ranged json export") {
                    Ok(rows) => rows,
                    Err(error) => panic!("{error}"),
                };
                assert_eq!(rows.len(), csv_rows.len(), "json selected different rows");
            }
            "packed" => {
                let document = match packed::decode(&response.body) {
                    Ok(document) => document,
                    Err(error) => panic!("a ranged packed export must decode: {error}"),
                };
                assert_same_table(
                    "a ranged packed export",
                    &csv_header,
                    &csv_rows,
                    &document.columns,
                    &document.rows,
                );
            }
            "arrow" => {
                #[cfg(feature = "arrow-export")]
                {
                    let (header, rows) = arrow_table(&response.body, "a ranged arrow export");
                    assert_same_table(
                        "a ranged arrow export",
                        &csv_header,
                        &csv_rows,
                        &header,
                        &rows,
                    );
                }
            }
            other => panic!("unhandled format {other}"),
        }
    }

    // And both ways to get a range wrong are typed rejections, in EVERY
    // format, naming the field a client can act on.
    for format in &offered.usable {
        for (what, query, field) in [
            (
                "an inverted range",
                format!("dataset=underlying&format={format}&from_step=2&to_step=1"),
                "from_step",
            ),
            (
                "a range past the end of the tape",
                format!("dataset=underlying&format={format}&from_step=0&to_step=99999"),
                "to_step",
            ),
        ] {
            let response = exported.export(&query);
            assert_eq!(
                response.status,
                400,
                "{what} must be refused as {format}, got {} with {}",
                response.status,
                response.text()
            );
            let content_type = response.header("content-type").unwrap_or_default();
            assert!(
                content_type.contains("application/json"),
                "a rejection is JSON whatever format was asked for, got {content_type:?}"
            );
            let body: serde_json::Value = match response.json(&query) {
                Ok(body) => body,
                Err(error) => {
                    panic!("{what} as {format} must answer the documented shape: {error}")
                }
            };
            assert_eq!(
                body.get("field").and_then(serde_json::Value::as_str),
                Some(field),
                "{what} as {format} must name {field}: {body}"
            );
            assert!(
                body.get("error")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|message| !message.is_empty()),
                "{what} as {format} must explain itself: {body}"
            );
        }
    }
}

/// Each greek level's header is a PREFIX of the next, so a consumer written
/// against `none` keeps reading the same columns in the same places.
#[test]
fn test_each_greek_level_extends_the_previous_one() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };

    let header_for = |level: &str| -> Vec<String> {
        let response = exported.export(&format!("dataset=option_chains&format=csv&greeks={level}"));
        assert_eq!(
            response.status,
            200,
            "the {level} greek level must export, got {}",
            response.text()
        );
        csv_table(&response.text(), &format!("the {level} greek level")).0
    };

    let (none, first, all) = (header_for("none"), header_for("first"), header_for("all"));
    assert!(
        first.starts_with(&none),
        "the first-order header must extend the none header, {none:?} then {first:?}"
    );
    assert!(
        all.starts_with(&first),
        "the all header must extend the first-order header, {first:?} then {all:?}"
    );
    assert!(
        all.len() > none.len(),
        "asking for greeks must actually add columns"
    );
}

/// A streamed export is complete: the last row of the tape is present and
/// carries every column the header declared.
#[test]
fn test_a_streamed_export_is_not_truncated() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };

    let response = exported.export("dataset=option_chains&format=csv&greeks=all");
    assert_eq!(response.status, 200, "{}", response.text());
    let text = response.text();
    let (header, rows) = csv_table(&text, "a full greek export");

    assert!(!rows.is_empty(), "a walked tape must export rows");
    assert!(
        text.ends_with("\r\n"),
        "a complete csv export ends with a line terminator, so a cut stream is visible"
    );

    let last = match rows.last() {
        Some(last) => last,
        None => unreachable!("rows was checked non-empty"),
    };
    assert_eq!(
        last.len(),
        header.len(),
        "the last row must carry every column the header declares"
    );
    if let Some(index) = header.iter().position(|column| column == "step") {
        let last_step: usize = match last.get(index).and_then(|value| value.parse().ok()) {
            Some(step) => step,
            None => panic!("the step column must carry a number, row was {last:?}"),
        };
        assert_eq!(
            last_step,
            STEPS - 1,
            "the export must reach the last step of the tape"
        );
    }
}

/// The content type and the suggested filename match the format asked for.
#[test]
fn test_the_content_type_and_filename_match_the_format() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let offered = offered_formats(&client, &exported);

    for format in &offered.usable {
        let response = exported.export(&format!("dataset=underlying&format={format}"));
        assert_eq!(response.status, 200);

        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_string();
        let expected_type = match format.as_str() {
            "json" => "application/json",
            "csv" => "text/csv",
            "arrow" => "application/vnd.apache.arrow.stream",
            _ => "application/octet-stream",
        };
        assert!(
            content_type.contains(expected_type),
            "{format} must be served as {expected_type}, got {content_type:?}"
        );

        let disposition = response
            .header("content-disposition")
            .unwrap_or_default()
            .to_string();
        assert!(
            disposition.contains("attachment") && disposition.contains(&exported.id),
            "{format} must suggest a filename naming the simulation, got {disposition:?}"
        );
        let expected_extension = match format.as_str() {
            "json" => ".json",
            "csv" => ".csv",
            "arrow" => ".arrow",
            _ => ".ocsp",
        };
        assert!(
            disposition.contains(expected_extension),
            "{format} must suggest a {expected_extension} file, got {disposition:?}"
        );
    }

    assert!(
        offered.absent.len() < 4,
        "no format at all is served by this deployment"
    );
}
