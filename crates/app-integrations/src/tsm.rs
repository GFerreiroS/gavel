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
    let raw = number(row, index, "updatedAt")?;
    // TSM's feeds use Unix seconds. Accept milliseconds too, so a feed-side
    // precision upgrade cannot move all observations to 1970.
    Ok(Millis(if raw < 100_000_000_000 {
        raw.saturating_mul(1_000)
    } else {
        raw
    }))
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
    fn parses_realistic_retail_feeds_and_filters_before_storage() {
        let commodities = "itemId,marketValue,minBuyout,quantity,historical,recent,updatedAt\n190311,123456,120000,17,110000,121000,1724133600\n190312,9,8,1,7,8,1724133600\n";
        let region_items = "itemId,marketValue,historical,avgSalePrice,saleRate,soldPerDay,updatedAt\n190311,123456,110000,119000,0.12345,42,1724133600\n190312,9,7,8,0.5,1,1724133600\n";
        let wanted = [ItemId(190311)];

        let commodity = parse_commodities(commodities, Region::Eu, &wanted).unwrap();
        let daily = parse_region_items(region_items, Region::Eu, &wanted).unwrap();

        assert_eq!(commodity.len(), 1);
        assert_eq!(commodity[0].min_buyout, Copper(120000));
        assert_eq!(commodity[0].observed_at, Millis(1_724_133_600_000));
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].sale_rate_bp, 1235);
        assert_eq!(daily[0].sold_per_day, 42);
    }

    #[test]
    fn accepts_tsm_region_column_aliases() {
        let csv = "itemId,regionMarketValue,regionHistorical,regionSaleAvg,regionSaleRate,regionSoldPerDay,updatedAt\n1,10,9,8,1,7,1724133600\n";
        let values = parse_region_items(csv, Region::Us, &[ItemId(1)]).unwrap();
        assert_eq!(values[0].sale_rate_bp, 10_000);
    }
}
