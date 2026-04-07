//! Finance tools — budget tracking, invoicing, P&L, cash flow, expense analysis.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct BudgetTrackerTool;
#[async_trait]
impl AgentTool for BudgetTrackerTool {
    fn name(&self) -> &str { "BudgetTracker" }
    fn description(&self) -> &str { "Track income and expenses. Categorize transactions, calculate totals, show spending breakdown." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"transactions":{"type":"array","items":{"type":"object","properties":{"amount":{"type":"number"},"category":{"type":"string"},"description":{"type":"string"},"type":{"type":"string","enum":["income","expense"]}}}},"period":{"type":"string","description":"Budget period (e.g. 'March 2026')"}},"required":["transactions"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let txns = input.get("transactions").and_then(Value::as_array).cloned().unwrap_or_default();
        let period = input.get("period").and_then(Value::as_str).unwrap_or("Current Period");
        let mut income = 0.0f64;
        let mut expenses = 0.0f64;
        let mut by_category: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for txn in &txns {
            let amount = txn.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
            let cat = txn.get("category").and_then(Value::as_str).unwrap_or("Uncategorized");
            let txn_type = txn.get("type").and_then(Value::as_str).unwrap_or("expense");
            if txn_type == "income" { income += amount; } else { expenses += amount; *by_category.entry(cat.to_string()).or_insert(0.0) += amount; }
        }
        let mut cats: Vec<_> = by_category.into_iter().collect();
        cats.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let cat_lines: Vec<String> = cats.iter().map(|(c, a)| format!("  {c}: ${a:.2} ({:.0}%)", a / expenses * 100.0)).collect();
        AgentToolResult::ok(format!("Budget Summary — {period}\n\n  Income:   ${income:.2}\n  Expenses: ${expenses:.2}\n  Net:      ${:.2}\n\nExpense Breakdown:\n{}", income - expenses, cat_lines.join("\n")))
    }
}

pub struct ProfitLossTool;
#[async_trait]
impl AgentTool for ProfitLossTool {
    fn name(&self) -> &str { "ProfitLoss" }
    fn description(&self) -> &str { "Calculate profit & loss statement. Revenue, COGS, gross profit, operating expenses, net income." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"revenue":{"type":"number"},"cogs":{"type":"number","description":"Cost of goods sold"},"operating_expenses":{"type":"number"},"other_income":{"type":"number"},"taxes":{"type":"number"},"period":{"type":"string"}},"required":["revenue"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let revenue = input.get("revenue").and_then(Value::as_f64).unwrap_or(0.0);
        let cogs = input.get("cogs").and_then(Value::as_f64).unwrap_or(0.0);
        let opex = input.get("operating_expenses").and_then(Value::as_f64).unwrap_or(0.0);
        let other = input.get("other_income").and_then(Value::as_f64).unwrap_or(0.0);
        let taxes = input.get("taxes").and_then(Value::as_f64).unwrap_or(0.0);
        let period = input.get("period").and_then(Value::as_str).unwrap_or("Current Period");
        let gross = revenue - cogs;
        let operating = gross - opex;
        let net = operating + other - taxes;
        let margin = if revenue > 0.0 { net / revenue * 100.0 } else { 0.0 };
        AgentToolResult::ok(format!("P&L Statement — {period}\n\n  Revenue:              ${revenue:>12.2}\n  Cost of Goods Sold:   ${cogs:>12.2}\n  Gross Profit:         ${gross:>12.2} ({:.0}%)\n  Operating Expenses:   ${opex:>12.2}\n  Operating Income:     ${operating:>12.2}\n  Other Income:         ${other:>12.2}\n  Taxes:                ${taxes:>12.2}\n  ─────────────────────────────\n  Net Income:           ${net:>12.2} ({margin:.1}% margin)", gross/revenue*100.0))
    }
}

pub struct CashFlowForecastTool;
#[async_trait]
impl AgentTool for CashFlowForecastTool {
    fn name(&self) -> &str { "CashFlowForecast" }
    fn description(&self) -> &str { "Project future cash flow based on recurring income and expenses over N months." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"starting_balance":{"type":"number"},"monthly_income":{"type":"number"},"monthly_expenses":{"type":"number"},"months":{"type":"number","description":"Months to project (default 12)"},"one_time_items":{"type":"array","items":{"type":"object","properties":{"month":{"type":"number"},"amount":{"type":"number"},"description":{"type":"string"}}}}},"required":["starting_balance","monthly_income","monthly_expenses"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let mut balance = input.get("starting_balance").and_then(Value::as_f64).unwrap_or(0.0);
        let income = input.get("monthly_income").and_then(Value::as_f64).unwrap_or(0.0);
        let expenses = input.get("monthly_expenses").and_then(Value::as_f64).unwrap_or(0.0);
        let months = input.get("months").and_then(Value::as_u64).unwrap_or(12) as usize;
        let one_time: Vec<Value> = input.get("one_time_items").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut lines = Vec::new();
        let net_monthly = income - expenses;
        for m in 1..=months {
            balance += net_monthly;
            for item in &one_time {
                if item.get("month").and_then(Value::as_u64) == Some(m as u64) {
                    let amt = item.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
                    let desc = item.get("description").and_then(Value::as_str).unwrap_or("");
                    balance += amt;
                    lines.push(format!("  Month {m:>2}: ${balance:>12.2}  ({desc}: ${amt:+.2})"));
                    continue;
                }
            }
            if !lines.last().map(|l: &String| l.contains(&format!("Month {m:>2}"))).unwrap_or(false) {
                lines.push(format!("  Month {m:>2}: ${balance:>12.2}"));
            }
        }
        let runway = if net_monthly < 0.0 && balance > 0.0 { format!("{:.0} months", balance / (-net_monthly)) } else { "N/A".into() };
        AgentToolResult::ok(format!("Cash Flow Forecast ({months} months)\n  Starting: ${:.2}\n  Monthly net: ${net_monthly:+.2}\n\n{}\n\n  Runway: {runway}", input.get("starting_balance").and_then(Value::as_f64).unwrap_or(0.0), lines.join("\n")))
    }
}

pub struct InvoiceGenTool;
#[async_trait]
impl AgentTool for InvoiceGenTool {
    fn name(&self) -> &str { "InvoiceGen" }
    fn description(&self) -> &str { "Generate a professional invoice from line items. Outputs formatted markdown." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"invoice_number":{"type":"string"},"from":{"type":"string","description":"Sender name/company"},"to":{"type":"string","description":"Recipient"},"items":{"type":"array","items":{"type":"object","properties":{"description":{"type":"string"},"quantity":{"type":"number"},"rate":{"type":"number"}}}},"tax_rate":{"type":"number","description":"Tax rate as decimal (e.g. 0.08 for 8%)"}},"required":["from","to","items"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let from = input.get("from").and_then(Value::as_str).unwrap_or("Company");
        let to = input.get("to").and_then(Value::as_str).unwrap_or("Client");
        let inv_num = input.get("invoice_number").and_then(Value::as_str).unwrap_or("INV-001");
        let items = input.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let tax_rate = input.get("tax_rate").and_then(Value::as_f64).unwrap_or(0.0);
        let mut subtotal = 0.0f64;
        let mut lines = Vec::new();
        for item in &items {
            let desc = item.get("description").and_then(Value::as_str).unwrap_or("Item");
            let qty = item.get("quantity").and_then(Value::as_f64).unwrap_or(1.0);
            let rate = item.get("rate").and_then(Value::as_f64).unwrap_or(0.0);
            let total = qty * rate;
            subtotal += total;
            lines.push(format!("| {desc} | {qty:.0} | ${rate:.2} | ${total:.2} |"));
        }
        let tax = subtotal * tax_rate;
        let grand = subtotal + tax;
        let date = chrono::Utc::now().format("%B %d, %Y");
        AgentToolResult::ok(format!("# Invoice {inv_num}\n\n**From:** {from}\n**To:** {to}\n**Date:** {date}\n\n| Description | Qty | Rate | Total |\n|-------------|-----|------|-------|\n{}\n\n**Subtotal:** ${subtotal:.2}\n**Tax ({:.0}%):** ${tax:.2}\n**Total Due:** ${grand:.2}", lines.join("\n"), tax_rate * 100.0))
    }
}
