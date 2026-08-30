//! The export matrix: every dataset in every format the deployment offers.
//!
//! The export is what a backtester actually consumes and it has the widest
//! surface in the service: three datasets, up to four encodings, a step range
//! and three greek levels. It also STREAMS, so its failure modes are the ones
//! that only appear over a socket, truncation first among them.
//!
//! Formats are probed rather than assumed. A deployment built without the
//! `arrow-export` feature refuses `arrow`, and an older one may not know
//! `packed` at all; both are facts to report, not failures. What the suite
//! guarantees is that everything the deployment DOES offer agrees with
//! everything else it offers.
//!
//! Exports are kept small, three steps over a two-strike chain, because these
//! run against a shared deployment and an export is the most expensive thing
//! it does.
//!
//! Skipped entirely when `OCS_INTEGRATION_BASE_URL` is unset.

use examples_integration::{Response, ServiceClient, reference_request, service};

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
            println!("SKIP: this deployment has no v2 API");
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
        if let Err(error) = self.client.delete(&path) {
            println!("WARNING: could not delete simulation {}: {error}", self.id);
        }
    }
}

/// Which formats this deployment actually serves, probed once.
///
/// `json` and `csv` are the baseline every build has; `arrow` needs a feature
/// that is off by default and `packed` postdates some deployments, so a 400 or
/// a 404 for those means "not offered here" and is reported rather than failed.
fn offered_formats(exported: &Exported) -> Vec<&'static str> {
    let mut offered = Vec::new();
    for format in ["json", "csv", "arrow", "packed"] {
        let response = exported.export(&format!("dataset=underlying&format={format}"));
        match response.status {
            200 => offered.push(format),
            400 | 404 | 415 => println!(
                "SKIP: this deployment does not offer the {format} encoding ({})",
                response.status
            ),
            other => panic!(
                "probing the {format} encoding answered {other}: {}",
                response.text()
            ),
        }
    }
    assert!(
        offered.contains(&"json") && offered.contains(&"csv"),
        "every build serves json and csv; this one offered {offered:?}"
    );
    offered
}

/// The rows of a CSV export, header first.
fn csv_rows(body: &str) -> Vec<Vec<String>> {
    body.split("\r\n")
        .filter(|line| !line.is_empty())
        .map(|line| line.split(',').map(str::to_string).collect())
        .collect()
}

/// The rows of a JSON export, as objects.
fn json_rows(body: &[u8], what: &str) -> Vec<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Array(rows)) => rows
            .into_iter()
            .map(|row| match row {
                serde_json::Value::Object(object) => object,
                other => panic!("{what}: every json row must be an object, got {other}"),
            })
            .collect(),
        Ok(other) => panic!("{what}: a json export must be a single array, got {other}"),
        Err(error) => panic!("{what}: a json export must parse: {error}"),
    }
}

/// The row count a packed document declares in its footer.
///
/// The footer is what makes a truncated download detectable, so reading it is
/// exactly the check a consumer must perform.
fn packed_rows(body: &[u8], what: &str) -> u64 {
    assert!(
        body.len() >= 16,
        "{what}: a packed document is at least a header and a footer, got {} bytes",
        body.len()
    );
    assert_eq!(
        &body[..4],
        b"OCSP",
        "{what}: a packed document starts with its magic"
    );

    // footer := u32:0xFFFFFFFF pad to 8 u64:total_rows, so the sentinel sits
    // 16 bytes from the end and the count in the last 8.
    let sentinel = &body[body.len() - 16..body.len() - 12];
    assert_eq!(
        sentinel,
        &u32::MAX.to_le_bytes(),
        "{what}: the footer sentinel is missing, so the download was truncated"
    );

    let mut count = [0_u8; 8];
    count.copy_from_slice(&body[body.len() - 8..]);
    u64::from_le_bytes(count)
}

/// Every dataset, in every offered format, carries the same rows.
#[test]
fn test_every_dataset_agrees_across_every_offered_format() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let formats = offered_formats(&exported);

    for dataset in DATASETS {
        let json = exported.export(&format!("dataset={dataset}&format=json"));
        assert_eq!(json.status, 200, "{dataset} as json: {}", json.text());
        let json_rows = json_rows(&json.body, dataset);
        assert!(
            !json_rows.is_empty(),
            "{dataset} must export at least one row for a walked tape"
        );

        let csv = exported.export(&format!("dataset={dataset}&format=csv"));
        assert_eq!(csv.status, 200, "{dataset} as csv: {}", csv.text());
        let text = csv.text();
        assert!(
            text.contains("\r\n"),
            "{dataset} as csv must use RFC 4180 CRLF endings"
        );
        let rows = csv_rows(&text);
        let (header, data) = match rows.split_first() {
            Some((header, data)) => (header, data),
            None => panic!("{dataset} as csv must carry a header"),
        };
        assert_eq!(
            data.len(),
            json_rows.len(),
            "{dataset} exports {} rows as csv and {} as json",
            data.len(),
            json_rows.len()
        );

        // Compared as parsed values rather than as text: the rendering of a
        // number is not the contract, the number is.
        for (index, (csv_row, json_row)) in data.iter().zip(&json_rows).enumerate() {
            for (column, value) in header.iter().zip(csv_row) {
                let Some(from_json) = json_row.get(column) else {
                    continue;
                };
                let matches = match from_json {
                    serde_json::Value::Number(number) => value.parse::<f64>().is_ok_and(|parsed| {
                        (parsed - number.as_f64().unwrap_or(f64::NAN)).abs() < 1e-9
                    }),
                    serde_json::Value::String(text) => text == value,
                    serde_json::Value::Null => value.is_empty(),
                    // Anything else, a bool say, renders the same way in both
                    // encodings, so its text is the comparison.
                    other => serde_json::to_string(other)
                        .is_ok_and(|rendered| rendered.trim_matches('"') == value),
                };
                assert!(
                    matches,
                    "{dataset} row {index} column {column} is {value:?} as csv and {from_json} \
                     as json"
                );
            }
        }

        if formats.contains(&"packed") {
            let packed = exported.export(&format!("dataset={dataset}&format=packed"));
            assert_eq!(packed.status, 200, "{dataset} as packed: {}", packed.text());
            let declared = packed_rows(&packed.body, dataset);
            assert_eq!(
                declared as usize,
                json_rows.len(),
                "{dataset} declares {declared} rows in its packed footer and exports {} as json",
                json_rows.len()
            );
        }

        if formats.contains(&"arrow") {
            let arrow = exported.export(&format!("dataset={dataset}&format=arrow"));
            assert_eq!(arrow.status, 200, "{dataset} as arrow: {}", arrow.text());
            assert!(
                !arrow.body.is_empty(),
                "{dataset} as arrow must carry a stream"
            );
        }
    }
}

/// The same export twice is byte-identical, which is the endpoint's stated
/// contract and what lets a consumer cache or checksum one.
#[test]
fn test_the_same_export_twice_is_byte_identical() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let formats = offered_formats(&exported);

    for dataset in DATASETS {
        for format in &formats {
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

/// A range selects the same rows in every format, and a bad one is a typed
/// rejection in every format rather than an empty document.
#[test]
fn test_a_range_selects_the_same_rows_in_every_format() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };
    let formats = offered_formats(&exported);

    let ranged = "dataset=underlying&from_step=1&to_step=2";
    let json = exported.export(&format!("{ranged}&format=json"));
    assert_eq!(json.status, 200, "{}", json.text());
    let expected = json_rows(&json.body, "underlying").len();
    assert!(
        expected > 0 && expected <= STEPS,
        "a range of two steps must select between one and {STEPS} rows, got {expected}"
    );

    let csv = exported.export(&format!("{ranged}&format=csv"));
    assert_eq!(
        csv_rows(&csv.text()).len() - 1,
        expected,
        "the same range must select the same rows as csv and as json"
    );

    if formats.contains(&"packed") {
        let packed = exported.export(&format!("{ranged}&format=packed"));
        assert_eq!(
            packed_rows(&packed.body, "a ranged underlying export") as usize,
            expected,
            "the same range must select the same rows as packed and as json"
        );
    }

    // And a bad range is a rejection in EVERY format, never an empty document
    // that a consumer would read as "no data".
    for format in &formats {
        let response = exported.export(&format!(
            "dataset=underlying&format={format}&from_step=2&to_step=1"
        ));
        assert_eq!(
            response.status,
            400,
            "an inverted range must be refused as {format}, got {} with {}",
            response.status,
            response.text()
        );
        let content_type = response.header("content-type").unwrap_or_default();
        assert!(
            content_type.contains("application/json"),
            "a rejection is JSON whatever format was asked for, got {content_type:?}"
        );
    }
}

/// Each greek level's header is a PREFIX of the next, so a consumer written
/// against `none` keeps reading the same columns in the same places when a
/// richer level is requested.
#[test]
fn test_each_greek_level_extends_the_previous_one() {
    let Some(client) = service() else {
        return;
    };
    let Some(exported) = Exported::create(&client) else {
        return;
    };

    let header_for = |level: &str| -> Option<Vec<String>> {
        let response = exported.export(&format!("dataset=option_chains&format=csv&greeks={level}"));
        if response.status == 400 {
            println!("SKIP: this deployment does not know the {level} greek level");
            return None;
        }
        assert_eq!(
            response.status,
            200,
            "the {level} greek level must export, got {}",
            response.text()
        );
        csv_rows(&response.text()).first().cloned()
    };

    let (Some(none), Some(first), Some(all)) =
        (header_for("none"), header_for("first"), header_for("all"))
    else {
        return;
    };

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

/// A streamed export is complete: the last row of the tape is present, and it
/// carries every column the header declared.
///
/// Truncation is the failure mode a socket introduces and an in-process test
/// cannot see.
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
    let rows = csv_rows(&text);
    let (header, data) = match rows.split_first() {
        Some((header, data)) => (header, data),
        None => panic!("a csv export must carry a header"),
    };

    assert!(!data.is_empty(), "a walked tape must export rows");
    assert!(
        text.ends_with("\r\n"),
        "a complete csv export ends with a line terminator, so a cut stream is visible"
    );

    let last = match data.last() {
        Some(last) => last,
        None => unreachable!("data was checked non-empty"),
    };
    assert_eq!(
        last.len(),
        header.len(),
        "the last row must carry every column the header declares, which is what a truncated \
         stream loses first"
    );

    // The last row belongs to the last step, so nothing at the end went
    // missing.
    let step_column = header.iter().position(|column| column == "step");
    if let Some(index) = step_column {
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
    let formats = offered_formats(&exported);

    for format in &formats {
        let response = exported.export(&format!("dataset=underlying&format={format}"));
        assert_eq!(response.status, 200);

        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_string();
        let expected_type = match *format {
            "json" => "application/json",
            "csv" => "text/csv",
            // Arrow has a registered media type of its own; only the packed
            // format falls back to plain octets.
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
        let expected_extension = match *format {
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
}
