//! TradeSkillMaster's public static CSV feeds.
//!
//! The adapter only parses and filters source data.  It intentionally has no
//! presentation concern: public visibility is a server configuration decision.

use std::collections::{BTreeMap, BTreeSet};

use app_core::error::{AppError, AppResult};
use app_core::market::{Copper, ItemId, Region, TsmCommoditySample, TsmRegionDaily};
use cluster_core::Millis;

const BASE_URL: &str = "https://public-data.tradeskillmaster.com";
const MAX_CSV_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct TsmClient {
    http: reqwest::Client,
    base_url: String,
}

impl TsmClient {
    pub fn new() -> AppResult<Self> {
        Self::with_base_url(BASE_URL)
    }

    fn with_base_url(base_url: &str) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("wow-auction-tracker/tsm-source")
            .build()
            .map_err(|e| AppError::internal(format!("building TSM HTTP client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Fetch the three-hour commodity feed, retaining only catalogue items.
    pub async fn commodities(
        &self,
        region: Region,
        wanted: &[ItemId],
    ) -> AppResult<Vec<TsmCommoditySample>> {
        let body = self
            .get(&format!("retail/{region}/commodities.csv"))
            .await?;
        parse_commodities(&body, region, wanted)
    }

    /// Fetch the daily regional completed-sales feed, retaining catalogue items.
    pub async fn region_items(
        &self,
        region: Region,
        wanted: &[ItemId],
    ) -> AppResult<Vec<TsmRegionDaily>> {
        let body = self
            .get(&format!("retail/{region}/region/items.csv"))
            .await?;
        parse_region_items(&body, region, wanted)
    }

    async fn get(&self, path: &str) -> AppResult<String> {
        let response = self
            .http
            .get(format!("{}/{path}", self.base_url))
            .send()
            .await
            .map_err(|e| AppError::Integration(format!("TSM {path} request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(AppError::Integration(format!(
                "TSM {path} returned HTTP {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| AppError::Integration(format!("reading TSM {path}: {e}")))?;
        if bytes.len() > MAX_CSV_BYTES {
            return Err(AppError::Integration(format!(
                "TSM {path} exceeds {} MiB",
                MAX_CSV_BYTES / (1024 * 1024)
            )));
        }
        String::from_utf8(bytes.to_vec())
            .map_err(|e| AppError::Integration(format!("TSM {path} is not UTF-8: {e}")))
    }
}

fn parse_commodities(
    csv: &str,
    region: Region,
    wanted: &[ItemId],
) -> AppResult<Vec<TsmCommoditySample>> {
    let table = CsvTable::new(csv)?;
    let wanted: BTreeSet<u32> = wanted.iter().map(|id| id.get()).collect();
    let item = table.required("itemId")?;
    let market_value = table.required_any(&["marketValue", "regionMarketValue"])?;
    let min_buyout = table.required("minBuyout")?;
    let recent = table.required("recent")?;
    let historical = table.required_any(&["historical", "regionHistorical"])?;
    let updated_at = table.required("updatedAt")?;
    let mut out = Vec::new();
    for row in table.rows {
        let id = number(&row, item, "itemId")? as u32;
        if !wanted.contains(&id) {
            continue;
        }
        let observed_at = timestamp(&row, updated_at)?;
        out.push(TsmCommoditySample {
            item: ItemId(id),
            region,
            observed_at,
            market_value: Copper(number(&row, market_value, "marketValue")?),
            min_buyout: Copper(number(&row, min_buyout, "minBuyout")?),
            recent: Copper(number(&row, recent, "recent")?),
            historical: Copper(number(&row, historical, "historical")?),
            updated_at: observed_at,
        });
    }
    same_updated_at(&out.iter().map(|row| row.updated_at).collect::<Vec<_>>())?;
    Ok(out)
}

fn parse_region_items(
    csv: &str,
    region: Region,
    wanted: &[ItemId],
) -> AppResult<Vec<TsmRegionDaily>> {
    let table = CsvTable::new(csv)?;
    let wanted: BTreeSet<u32> = wanted.iter().map(|id| id.get()).collect();
    let item = table.required("itemId")?;
    let market_value = table.required_any(&["marketValue", "regionMarketValue"])?;
    let historical = table.required_any(&["historical", "regionHistorical"])?;
    let avg_sale_price = table.required_any(&["avgSalePrice", "regionSaleAvg"])?;
    let sale_rate = table.required_any(&["saleRate", "regionSaleRate"])?;
    let sold_per_day = table.required_any(&["soldPerDay", "regionSoldPerDay"])?;
    let updated_at = table.required("updatedAt")?;
    let mut out = Vec::new();
    for row in table.rows {
        let id = number(&row, item, "itemId")? as u32;
        if !wanted.contains(&id) {
            continue;
        }
        let updated_at = timestamp(&row, updated_at)?;
        out.push(TsmRegionDaily {
            item: ItemId(id),
            region,
            day: utc_day(updated_at),
            market_value: Copper(number(&row, market_value, "marketValue")?),
            historical: Copper(number(&row, historical, "historical")?),
            avg_sale_price: Copper(number(&row, avg_sale_price, "avgSalePrice")?),
            sale_rate_bp: basis_points(value(&row, sale_rate, "saleRate")?)?,
            sold_per_day: number(&row, sold_per_day, "soldPerDay")?,
            updated_at,
        });
    }
    same_updated_at(&out.iter().map(|row| row.updated_at).collect::<Vec<_>>())?;
    Ok(out)
}

struct CsvTable {
    columns: BTreeMap<String, usize>,
    rows: Vec<Vec<String>>,
}

impl CsvTable {
    fn new(input: &str) -> AppResult<Self> {
        let mut lines = input.lines().filter(|line| !line.trim().is_empty());
        let header = lines
            .next()
            .ok_or_else(|| AppError::Integration("TSM CSV has no header".into()))?;
        let mut columns = BTreeMap::new();
        for (index, name) in csv_fields(header).into_iter().enumerate() {
            columns.insert(name.trim_start_matches('\u{feff}').to_string(), index);
        }
        Ok(Self {
            columns,
            rows: lines.map(csv_fields).collect(),
        })
    }

    fn required(&self, name: &str) -> AppResult<usize> {
        self.columns
            .get(name)
            .copied()
            .ok_or_else(|| AppError::Integration(format!("TSM CSV is missing {name}")))
    }

    fn required_any(&self, names: &[&str]) -> AppResult<usize> {
        names
            .iter()
            .find_map(|name| self.columns.get(*name).copied())
            .ok_or_else(|| {
                AppError::Integration(format!("TSM CSV is missing {}", names.join(" or ")))
            })
    }
}

/// The feeds are numeric, but this accepts quoted values too so a later name
/// column cannot shift the fields we use.
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

fn value<'a>(row: &'a [String], index: usize, column: &str) -> AppResult<&'a str> {
    row.get(index)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Integration(format!("TSM row has no {column}")))
}

fn number(row: &[String], index: usize, column: &str) -> AppResult<u64> {
    value(row, index, column)?
        .parse()
        .map_err(|e| AppError::Integration(format!("TSM {column} is not an unsigned integer: {e}")))
}

fn timestamp(row: &[String], index: usize) -> AppResult<Millis> {
    let raw = value(row, index, "updatedAt")?;
    if let Some(at) = rfc3339_utc(raw) {
        return Ok(at);
    }
    if let Ok(raw) = raw.parse::<u64>() {
        // Numeric feeds may use Unix seconds or milliseconds. Accept both so
        // a feed-side precision upgrade cannot move observations to 1970.
        return Ok(Millis(if raw < 100_000_000_000 {
            raw.saturating_mul(1_000)
        } else {
            raw
        }));
    }

    let shown: String = raw.chars().take(120).collect();
    Err(AppError::Integration(format!(
        "TSM updatedAt is invalid; expected RFC 3339 UTC or Unix seconds/milliseconds, got {shown:?}"
    )))
}

/// Parse TSM's fixed UTC RFC 3339 form (`YYYY-MM-DDTHH:MM:SSZ`) without a
/// date dependency. Reject variants rather than risk converting them wrongly.
fn rfc3339_utc(raw: &str) -> Option<Millis> {
    let bytes = raw.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }

    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])? as u32;
    let day = digits(&bytes[8..10])? as u32;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let midnight = Millis::from_utc_date(year as i64, month, day).get();
    let offset = (hour * 3_600 + minute * 60 + second).checked_mul(1_000)?;
    midnight.checked_add(offset).map(Millis)
}

fn digits(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u64::from(byte - b'0'))
    })
}

fn days_in_month(year: u64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn utc_day(at: Millis) -> Millis {
    Millis(at.get() / 86_400_000 * 86_400_000)
}

fn basis_points(value: &str) -> AppResult<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole: u16 = whole
        .parse()
        .map_err(|e| AppError::Integration(format!("TSM saleRate is invalid: {e}")))?;
    if whole > 1 || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::Integration(
            "TSM saleRate is outside 0..=1".into(),
        ));
    }
    let mut scaled = fraction.chars().take(4).collect::<String>();
    while scaled.len() < 4 {
        scaled.push('0');
    }
    let mut bp = whole * 10_000 + scaled.parse::<u16>().unwrap_or(0);
    if fraction.chars().nth(4).is_some_and(|digit| digit >= '5') {
        bp = bp.saturating_add(1);
    }
    (bp <= 10_000)
        .then_some(bp)
        .ok_or_else(|| AppError::Integration("TSM saleRate is outside 0..=1".into()))
}

fn same_updated_at(values: &[Millis]) -> AppResult<()> {
    if values.windows(2).all(|pair| pair[0] == pair[1]) {
        Ok(())
    } else {
        Err(AppError::Integration(
            "TSM file has inconsistent updatedAt values".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_tsm_iso_8601_feeds_and_filters_before_storage() {
        let commodities = "itemId,name,marketValue,minBuyout,recent,historical,updatedAt\n222505,Ironclaw Razorstone,32787241,39900000,39900000,1800400,2026-09-04T05:23:31Z\n241311,Haranir Phial of Finesse,198929,230000,230000,155500,2026-09-04T05:23:31Z\n";
        let region_items = "itemId,name,marketValue,historical,avgSalePrice,saleRate,soldPerDay,updatedAt\n2824,Hurricane,157894953,169722325,9499050,0.013,0,2026-09-03T01:35:57Z\n";
        let commodity_wanted = [ItemId(222505)];
        let region_wanted = [ItemId(2824)];

        let commodity = parse_commodities(commodities, Region::Eu, &commodity_wanted).unwrap();
        let daily = parse_region_items(region_items, Region::Eu, &region_wanted).unwrap();

        assert_eq!(commodity.len(), 1);
        assert_eq!(commodity[0].min_buyout, Copper(39_900_000));
        assert_eq!(commodity[0].observed_at, Millis(1_788_499_411_000));
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].sale_rate_bp, 130);
        assert_eq!(daily[0].sold_per_day, 0);
        assert_eq!(daily[0].day, Millis(1_788_393_600_000));
    }

    #[test]
    fn accepts_unix_seconds_and_milliseconds_updated_at() {
        let seconds = vec!["1724133600".to_string()];
        let milliseconds = vec!["1724133600000".to_string()];

        assert_eq!(timestamp(&seconds, 0).unwrap(), Millis(1_724_133_600_000));
        assert_eq!(
            timestamp(&milliseconds, 0).unwrap(),
            Millis(1_724_133_600_000)
        );
    }

    #[test]
    fn rejects_malformed_updated_at_with_its_value() {
        let row = vec!["2026-09-04 05:23:31Z".to_string()];
        let error = timestamp(&row, 0).unwrap_err().to_string();

        assert!(error.contains("updatedAt is invalid"));
        assert!(error.contains("\"2026-09-04 05:23:31Z\""));
    }

    #[test]
    fn accepts_tsm_region_column_aliases() {
        let csv = "itemId,regionMarketValue,regionHistorical,regionSaleAvg,regionSaleRate,regionSoldPerDay,updatedAt\n1,10,9,8,1,7,1724133600\n";
        let values = parse_region_items(csv, Region::Us, &[ItemId(1)]).unwrap();
        assert_eq!(values[0].sale_rate_bp, 10_000);
    }
}
