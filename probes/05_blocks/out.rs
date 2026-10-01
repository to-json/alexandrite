//! Probe 05, hand-emitted twice:
//!   `closures`: block -> Rust closure. Non-local control flow needs
//!               ControlFlow plumbing.
//!   `inlined`:  block passed to a known Enumerable method -> loop body
//!               (Kotlin inline-lambda style). return/?/break/next are
//!               plain Rust control flow.
//! Both must agree on every input.

use std::collections::HashMap;
use std::ops::ControlFlow;

#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    id: i64,
    customer: String,
    total: i64,
}

#[derive(Debug, PartialEq)]
pub struct ParseError(String);

fn parse(line: &str) -> Result<Order, ParseError> {
    let bad = || ParseError(line.to_owned());
    let mut f = line.split(',');
    let id = f.next().and_then(|s| s.trim().parse().ok()).ok_or_else(bad)?;
    let customer = f.next().map(|s| s.trim().to_owned()).ok_or_else(bad)?;
    let total = f.next().and_then(|s| s.trim().parse().ok()).ok_or_else(bad)?;
    Ok(Order { id, customer, total })
}

mod closures {
    use super::*;

    pub fn parse_all(lines: &[&str]) -> Result<Vec<Order>, ParseError> {
        // Only works because Result: FromIterator. Generally, `?` in a
        // block needs the same try_fold dance as `return` below.
        lines.iter().map(|it| parse(it)).collect::<Result<Vec<_>, _>>()
    }

    pub fn first_big_spender(orders: &[Order], limit: i64) -> Option<String> {
        let mut order: Vec<&str> = Vec::new();
        let mut groups: HashMap<&str, Vec<&Order>> = HashMap::new();
        orders.iter().for_each(|o| {
            groups
                .entry(&o.customer)
                .or_insert_with(|| {
                    order.push(&o.customer);
                    Vec::new()
                })
                .push(o)
        });
        let flow = order.iter().try_for_each(|customer| {
            let os = &groups[customer];
            if os.iter().map(|o| o.total).sum::<i64>() > limit {
                return ControlFlow::Break(customer.to_string());
            }
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(v) = flow {
            return Some(v);
        }
        None
    }

    pub fn top_totals(orders: &[Order]) -> Vec<i64> {
        orders.iter().filter(|it| it.total > 100).map(|o| o.total).take(3).collect::<Vec<_>>()
    }

    pub fn first_gap(orders: &[Order]) -> Option<i64> {
        let flow = orders.windows(2).try_for_each(|w| {
            let (a, b) = (&w[0], &w[1]);
            if b.id == a.id + 1 {
                return ControlFlow::Continue(());
            }
            ControlFlow::Break(a.id + 1)
        });
        match flow {
            ControlFlow::Break(v) => Some(v),
            ControlFlow::Continue(()) => None,
        }
    }
}

mod inlined {
    use super::*;

    pub fn parse_all(lines: &[&str]) -> Result<Vec<Order>, ParseError> {
        let mut out = Vec::with_capacity(lines.len());
        for it in lines {
            out.push(parse(it)?);
        }
        Ok(out)
    }

    pub fn first_big_spender(orders: &[Order], limit: i64) -> Option<String> {
        // group_by: insertion-ordered, like a Ruby Hash.
        let mut index: HashMap<&str, usize> = HashMap::new();
        let mut groups: Vec<(&str, Vec<&Order>)> = Vec::new();
        for o in orders {
            let i = *index.entry(&o.customer).or_insert_with(|| {
                groups.push((&o.customer, Vec::new()));
                groups.len() - 1
            });
            groups[i].1.push(o);
        }
        for (customer, os) in &groups {
            let mut sum = 0;
            for o in os {
                sum += o.total;
            }
            if sum > limit {
                return Some(customer.to_string());
            }
        }
        None
    }

    pub fn top_totals(orders: &[Order]) -> Vec<i64> {
        let mut out = Vec::with_capacity(3);
        for it in orders {
            if !(it.total > 100) {
                continue;
            }
            out.push(it.total);
            if out.len() == 3 {
                break;
            }
        }
        out
    }

    pub fn first_gap(orders: &[Order]) -> Option<i64> {
        // each_cons's value is nil unless `break` supplies one. A labeled
        // block is exactly "a call whose value break can set".
        'each_cons: {
            for w in orders.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                if b.id == a.id + 1 {
                    continue;
                }
                break 'each_cons Some(a.id + 1);
            }
            None
        }
    }
}

fn main() {
    let good = ["1, ann, 50", "2, bo, 300", "3, ann, 120", "5, cy, 90", "6, bo, 400", "7, ann, 900"];
    let bad = ["1, ann, 50", "2, bo, lots", "3, ann, 120"];

    let o = closures::parse_all(&good).unwrap();
    assert_eq!(o, inlined::parse_all(&good).unwrap());
    let e = closures::parse_all(&bad);
    assert_eq!(e, inlined::parse_all(&bad));

    let cases = [
        ("first_big_spender(600)", format!("{:?}", closures::first_big_spender(&o, 600)), format!("{:?}", inlined::first_big_spender(&o, 600))),
        ("first_big_spender(9999)", format!("{:?}", closures::first_big_spender(&o, 9999)), format!("{:?}", inlined::first_big_spender(&o, 9999))),
        ("top_totals", format!("{:?}", closures::top_totals(&o)), format!("{:?}", inlined::top_totals(&o))),
        ("first_gap", format!("{:?}", closures::first_gap(&o)), format!("{:?}", inlined::first_gap(&o))),
        ("first_gap(no gap)", format!("{:?}", closures::first_gap(&o[..3])), format!("{:?}", inlined::first_gap(&o[..3]))),
    ];
    println!("parse_all(bad) = {e:?}");
    for (name, c, i) in cases {
        assert_eq!(c, i, "{name}");
        println!("{name:<24} = {c}");
    }
    println!("closures and inlined agree on all cases");
}
