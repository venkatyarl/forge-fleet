//! Analytics & data tools — statistics, time series, data visualization.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct StatsCalcTool;
#[async_trait]
impl AgentTool for StatsCalcTool {
    fn name(&self) -> &str { "StatsCalc" }
    fn description(&self) -> &str { "Calculate statistics: mean, median, mode, std deviation, percentiles, min/max, correlation." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"data":{"type":"array","items":{"type":"number"}},"data_b":{"type":"array","items":{"type":"number"},"description":"Second dataset for correlation"},"percentiles":{"type":"array","items":{"type":"number"},"description":"Percentiles to calculate (e.g. [25, 50, 75, 90, 99])"}},"required":["data"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let mut data: Vec<f64> = input.get("data").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        if data.is_empty() { return AgentToolResult::err("No data provided"); }
        data.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = data.len() as f64;
        let sum: f64 = data.iter().sum();
        let mean = sum / n;
        let median = if data.len() % 2 == 0 { (data[data.len()/2 - 1] + data[data.len()/2]) / 2.0 } else { data[data.len()/2] };
        let variance = data.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        let min = data[0];
        let max = data[data.len() - 1];

        let mut output = format!("Statistics (n={}):\n  Mean:   {mean:.4}\n  Median: {median:.4}\n  Std Dev: {std_dev:.4}\n  Min:    {min:.4}\n  Max:    {max:.4}\n  Range:  {:.4}", data.len(), max - min);

        if let Some(pcts) = input.get("percentiles").and_then(Value::as_array) {
            output.push_str("\n\n  Percentiles:");
            for p in pcts.iter().filter_map(Value::as_f64) {
                let idx = ((p / 100.0) * (data.len() as f64 - 1.0)).round() as usize;
                output.push_str(&format!("\n    P{:.0}: {:.4}", p, data[idx.min(data.len() - 1)]));
            }
        }

        if let Some(data_b) = input.get("data_b").and_then(Value::as_array) {
            let b: Vec<f64> = data_b.iter().filter_map(Value::as_f64).collect();
            if b.len() == data.len() {
                let mean_b = b.iter().sum::<f64>() / b.len() as f64;
                let cov = data.iter().zip(b.iter()).map(|(a, b)| (a - mean) * (b - mean_b)).sum::<f64>() / n;
                let std_b = (b.iter().map(|x| (x - mean_b).powi(2)).sum::<f64>() / n).sqrt();
                let corr = if std_dev * std_b > 0.0 { cov / (std_dev * std_b) } else { 0.0 };
                output.push_str(&format!("\n\n  Correlation with data_b: {corr:.4}"));
            }
        }

        AgentToolResult::ok(output)
    }
}

pub struct TimeSeriesAnalysisTool;
#[async_trait]
impl AgentTool for TimeSeriesAnalysisTool {
    fn name(&self) -> &str { "TimeSeriesAnalysis" }
    fn description(&self) -> &str { "Analyze time series data: detect trends, calculate moving averages, identify outliers." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"values":{"type":"array","items":{"type":"number"}},"labels":{"type":"array","items":{"type":"string"},"description":"Labels for each data point (dates, etc.)"},"window":{"type":"number","description":"Moving average window (default 3)"}},"required":["values"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let values: Vec<f64> = input.get("values").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        if values.len() < 3 { return AgentToolResult::err("Need at least 3 data points"); }
        let window = input.get("window").and_then(Value::as_u64).unwrap_or(3) as usize;
        let labels: Vec<String> = input.get("labels").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_else(|| (1..=values.len()).map(|i| format!("{i}")).collect());

        // Moving average
        let mut ma = Vec::new();
        for i in 0..values.len() {
            if i + 1 >= window {
                let avg: f64 = values[i+1-window..=i].iter().sum::<f64>() / window as f64;
                ma.push(avg);
            }
        }

        // Trend (linear regression slope)
        let n = values.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = values.iter().sum::<f64>() / n;
        let slope = values.iter().enumerate()
            .map(|(i, y)| (i as f64 - x_mean) * (y - y_mean)).sum::<f64>()
            / values.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum::<f64>();
        let trend = if slope > 0.01 { "upward" } else if slope < -0.01 { "downward" } else { "flat" };

        // Outliers (> 2 std devs from mean)
        let std_dev = (values.iter().map(|x| (x - y_mean).powi(2)).sum::<f64>() / n).sqrt();
        let outliers: Vec<String> = values.iter().enumerate()
            .filter(|(_, v)| (**v - y_mean).abs() > 2.0 * std_dev)
            .map(|(i, v)| format!("  {} = {v:.2} ({:.1}σ)", labels.get(i).unwrap_or(&format!("{i}")), (v - y_mean) / std_dev))
            .collect();

        AgentToolResult::ok(format!(
            "Time Series Analysis ({} points):\n  Trend: {trend} (slope: {slope:.4})\n  Mean: {y_mean:.2}\n  Moving avg (window={window}): last = {:.2}\n\nOutliers (>2σ):\n{}\n",
            values.len(), ma.last().unwrap_or(&0.0),
            if outliers.is_empty() { "  None".into() } else { outliers.join("\n") }
        ))
    }
}
