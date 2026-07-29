// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! gnubgapi results mapped onto the protocol's contract payloads —
//! semantics mirrored from GammonBase's GnuBgApiEvaluator so the gnubg
//! engine family produces identical rows on desktop and in the cloud.

use bep_protocol::contract::{
    cube_action, sanitize, CubeEvaluation, MoveAnalysis, MoveHint, MovesEvaluation,
    PositionEvaluation, CUBEFUL_MAX, CUBEFUL_MIN, EQUITY_MAX, EQUITY_MIN, PROB_MAX, PROB_MIN,
};
use bep_protocol::gnubg_ids::position_id_storage_base64;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::ffi::{cube_decision, equity_index, outputs, CubeDecisionResult, ScoredMove};

pub const BAR_TOKEN: &str = "bar";
pub const OFF_TOKEN: &str = "off";
/// gnubgapi an_move convention: 0-indexed points, bar = 24, off < 0.
const AN_MOVE_BAR: i32 = 24;

fn sanitize_logged(value: f64, lo: f64, hi: f64, field: &str, position_id: &str) -> f64 {
    if value.is_nan() {
        eprintln!("gnubg returned NaN for {field} on position {position_id} — clamping to 0");
    } else if value.is_infinite() {
        eprintln!("gnubg returned {value} for {field} on position {position_id} — clamping");
    }
    sanitize(value, lo, hi)
}

/// GnubgMove.ToNotation(): plain per-hop "from/to", 1-indexed, lowercase
/// bar/off — already the contract's normalized shape (gnubg never merges
/// hops, stars hits, or groups repeats in an_move form).
pub fn notation_from_an_move(an_move: &[i32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 1 < an_move.len() {
        let (from, to) = (an_move[i], an_move[i + 1]);
        if from < 0 {
            break;
        }
        let f = if from == AN_MOVE_BAR { BAR_TOKEN.to_string() } else { (from + 1).to_string() };
        let t = if to < 0 { OFF_TOKEN.to_string() } else { (to + 1).to_string() };
        parts.push(format!("{f}/{t}"));
        i += 2;
    }
    parts.join(" ")
}

/// Canonical form for played-move matching: lowercase, hit stars
/// stripped, hop tokens SORTED. gnubg orders hops by die order while
/// callers commonly write highest-point-first — the play is the same
/// multiset of hops, so comparison must not depend on token order.
/// (The cloud evaluator's order-sensitive string compare never bit
/// because its callers echo gnubg-produced strings back; a protocol
/// host sends notation from anywhere.)
pub fn normalize_notation(notation: &str) -> String {
    let mut tokens: Vec<String> = notation
        .split_whitespace()
        .map(|token| {
            token.chars().filter(|c| *c != '*').collect::<String>().to_ascii_lowercase()
        })
        .collect();
    tokens.sort_unstable();
    tokens.join(" ")
}

/// GnuBgApiEvaluator.MapDecision: collapse gnubg's 21 cubedecision
/// values to (RecommendedAction, TooGoodToDouble).
pub fn map_decision(decision: i32) -> Result<(i32, bool), String> {
    match decision {
        cube_decision::DOUBLE_TAKE
        | cube_decision::DOUBLE_PASS
        | cube_decision::DOUBLE_BEAVER
        | cube_decision::REDOUBLE_TAKE
        | cube_decision::REDOUBLE_PASS
        | cube_decision::OPTIONAL_DOUBLE_TAKE
        | cube_decision::OPTIONAL_REDOUBLE_TAKE
        | cube_decision::OPTIONAL_DOUBLE_BEAVER
        | cube_decision::OPTIONAL_DOUBLE_PASS
        | cube_decision::OPTIONAL_REDOUBLE_PASS => Ok((cube_action::DOUBLE, false)),

        cube_decision::NODOUBLE_TAKE
        | cube_decision::NODOUBLE_BEAVER
        | cube_decision::NO_REDOUBLE_TAKE
        | cube_decision::NO_REDOUBLE_BEAVER
        | cube_decision::NODOUBLE_DEADCUBE
        | cube_decision::NO_REDOUBLE_DEADCUBE
        | cube_decision::NOT_AVAILABLE => Ok((cube_action::NO_DOUBLE, false)),

        cube_decision::TOOGOOD_TAKE
        | cube_decision::TOOGOOD_PASS
        | cube_decision::TOOGOODRE_TAKE
        | cube_decision::TOOGOODRE_PASS => Ok((cube_action::NO_DOUBLE, true)),

        other => Err(format!("unrecognised cubedecision value {other} from gnubg")),
    }
}

pub fn position_payload(output: &[f64; 7], position_id: &str) -> PositionEvaluation {
    PositionEvaluation {
        equity: sanitize_logged(
            output[outputs::CUBELESS],
            EQUITY_MIN,
            EQUITY_MAX,
            "Equity",
            position_id,
        ),
        cubeful_equity: sanitize_logged(
            output[outputs::CUBEFUL],
            CUBEFUL_MIN,
            CUBEFUL_MAX,
            "CubefulEquity",
            position_id,
        ),
        win_prob: sanitize_logged(output[outputs::WIN], PROB_MIN, PROB_MAX, "WinProb", position_id),
        win_gammon: sanitize_logged(
            output[outputs::WIN_GAMMON],
            PROB_MIN,
            PROB_MAX,
            "WinGammon",
            position_id,
        ),
        win_backgammon: sanitize_logged(
            output[outputs::WIN_BACKGAMMON],
            PROB_MIN,
            PROB_MAX,
            "WinBackgammon",
            position_id,
        ),
        lose_gammon: sanitize_logged(
            output[outputs::LOSE_GAMMON],
            PROB_MIN,
            PROB_MAX,
            "LoseGammon",
            position_id,
        ),
        lose_backgammon: sanitize_logged(
            output[outputs::LOSE_BACKGAMMON],
            PROB_MIN,
            PROB_MAX,
            "LoseBackgammon",
            position_id,
        ),
    }
}

pub fn cube_payload(
    result: &CubeDecisionResult,
    position_id: &str,
) -> Result<CubeEvaluation, String> {
    let no_double = sanitize_logged(
        result.equities[equity_index::NODOUBLE],
        CUBEFUL_MIN,
        CUBEFUL_MAX,
        "NoDoubleEquity",
        position_id,
    );
    let take = sanitize_logged(
        result.equities[equity_index::TAKE],
        CUBEFUL_MIN,
        CUBEFUL_MAX,
        "TakeEquity",
        position_id,
    );
    let drop = sanitize_logged(
        result.equities[equity_index::DROP],
        CUBEFUL_MIN,
        CUBEFUL_MAX,
        "DropEquity",
        position_id,
    );
    let (action, too_good) = map_decision(result.decision)?;

    // Row 0 = no-double scenario ("Our"), row 1 = double-take scenario
    // ("Opp") — the cloud evaluator's slicing, offerer's W/G/B in both.
    let no_double_row = &result.cubeful_outputs[0];
    let take_row = &result.cubeful_outputs[1];
    let prob = |value: f64, field: &str| {
        Some(sanitize_logged(value, PROB_MIN, PROB_MAX, field, position_id))
    };
    Ok(CubeEvaluation {
        recommended_action: action,
        no_double_equity: no_double,
        take_equity: take,
        drop_equity: drop,
        double_take_gain: take - no_double,
        double_drop_gain: drop - no_double,
        too_good_to_double: too_good,
        // gnubg's live-cube efficiency is not exposed by gnubgapi.
        cube_efficiency: None,
        our_win_prob: prob(no_double_row[outputs::WIN], "NoDouble.WinProb"),
        our_gammon_prob: prob(no_double_row[outputs::WIN_GAMMON], "NoDouble.WinGammon"),
        our_backgammon_prob: prob(no_double_row[outputs::WIN_BACKGAMMON], "NoDouble.WinBackgammon"),
        opp_win_prob: prob(take_row[outputs::WIN], "DoubleTake.WinProb"),
        opp_gammon_prob: prob(take_row[outputs::WIN_GAMMON], "DoubleTake.WinGammon"),
        opp_backgammon_prob: prob(take_row[outputs::WIN_BACKGAMMON], "DoubleTake.WinBackgammon"),
    })
}

pub fn move_hints(
    scored: &[ScoredMove],
    position_id: &str,
    match_id: &str,
    die1: i32,
    die2: i32,
    plies_stamp: i32,
) -> Result<MovesEvaluation, String> {
    let storage_id = position_id_storage_base64(position_id).map_err(|e| e.to_string())?;
    let (sorted_die1, sorted_die2) = if die1 <= die2 { (die1, die2) } else { (die2, die1) };
    let evaluated_utc = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|e| {
        eprintln!("failed to format EvaluatedUtc: {e}");
        String::new()
    });

    let best_equity = scored.first().map_or(0.0, |m| {
        sanitize_logged(m.equity, EQUITY_MIN, EQUITY_MAX, "MoveEquity[best]", position_id)
    });

    let alternatives = scored
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let equity =
                sanitize_logged(entry.equity, EQUITY_MIN, EQUITY_MAX, "MoveEquity", position_id);
            MoveHint {
                gnubg_position_id: storage_id.clone(),
                gnubg_match_id: match_id.to_string(),
                die1: sorted_die1,
                die2: sorted_die2,
                evaluation_engine_id: 0,
                plies: plies_stamp,
                rank: (index + 1) as i32,
                move_notation: notation_from_an_move(&entry.mv.an_move),
                equity,
                error_vs_best: best_equity - equity,
                win_prob: sanitize_logged(entry.probs[0], PROB_MIN, PROB_MAX, "WinProb", position_id),
                win_gammon: sanitize_logged(entry.probs[1], PROB_MIN, PROB_MAX, "WinGammon", position_id),
                win_backgammon: sanitize_logged(entry.probs[2], PROB_MIN, PROB_MAX, "WinBackgammon", position_id),
                lose_gammon: sanitize_logged(entry.probs[3], PROB_MIN, PROB_MAX, "LoseGammon", position_id),
                lose_backgammon: sanitize_logged(entry.probs[4], PROB_MIN, PROB_MAX, "LoseBackgammon", position_id),
                evaluated_utc: evaluated_utc.clone(),
            }
        })
        .collect();

    Ok(MovesEvaluation { die1: sorted_die1, die2: sorted_die2, alternatives })
}

/// GnuBgApiEvaluator.AnalyzeMove: identify the played move by normalized
/// notation; synthesize played = best when not found.
pub fn analyze_payload(moves: MovesEvaluation, played_notation: &str) -> Result<MoveAnalysis, String> {
    let best = moves
        .alternatives
        .first()
        .cloned()
        .ok_or_else(|| "no ranked alternatives".to_string())?;
    let wanted = normalize_notation(played_notation);
    let played = moves
        .alternatives
        .iter()
        .find(|hint| normalize_notation(&hint.move_notation) == wanted)
        .cloned()
        .unwrap_or_else(|| best.clone());
    Ok(MoveAnalysis { played, best })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notation_formats_bar_off_and_points() {
        // 24/23 13/10 in gnubgapi form: 0-indexed (23,22) and (12,9).
        assert_eq!(notation_from_an_move(&[23, 22, 12, 9, -1, -1, -1, -1]), "24/23 13/10");
        // bar entry and bear-off.
        assert_eq!(notation_from_an_move(&[24, 21, 5, -1, -1, -1, -1, -1]), "bar/22 6/off");
        // no legal move.
        assert_eq!(notation_from_an_move(&[-1; 8]), "");
    }

    #[test]
    fn normalize_is_case_star_and_order_insensitive() {
        assert_eq!(normalize_notation("Bar/22* 13/10"), normalize_notation("13/10 bar/22"));
        assert_ne!(normalize_notation("24/23 13/10"), normalize_notation("24/21 13/12"));
    }

    #[test]
    fn decision_collapse_matches_the_cloud_evaluator() {
        assert_eq!(map_decision(cube_decision::DOUBLE_PASS).unwrap(), (cube_action::DOUBLE, false));
        assert_eq!(
            map_decision(cube_decision::OPTIONAL_REDOUBLE_PASS).unwrap(),
            (cube_action::DOUBLE, false)
        );
        assert_eq!(
            map_decision(cube_decision::NOT_AVAILABLE).unwrap(),
            (cube_action::NO_DOUBLE, false)
        );
        assert_eq!(
            map_decision(cube_decision::TOOGOODRE_PASS).unwrap(),
            (cube_action::NO_DOUBLE, true)
        );
        assert!(map_decision(99).is_err());
    }

    #[test]
    fn analyze_falls_back_to_best_when_unmatched() {
        let hint = |rank: i32, notation: &str| MoveHint {
            gnubg_position_id: String::new(),
            gnubg_match_id: String::new(),
            die1: 1,
            die2: 3,
            evaluation_engine_id: 0,
            plies: 0,
            rank,
            move_notation: notation.to_string(),
            equity: 0.0,
            error_vs_best: 0.0,
            win_prob: 0.5,
            win_gammon: 0.0,
            win_backgammon: 0.0,
            lose_gammon: 0.0,
            lose_backgammon: 0.0,
            evaluated_utc: String::new(),
        };
        let moves = MovesEvaluation {
            die1: 1,
            die2: 3,
            alternatives: vec![hint(1, "8/5 6/5"), hint(2, "24/23 13/10")],
        };
        let matched = analyze_payload(moves.clone(), "24/23 13/10").unwrap();
        assert_eq!(matched.played.rank, 2);
        let fallback = analyze_payload(moves, "3/2 1/off").unwrap();
        assert_eq!(fallback.played.rank, 1);
    }
}
