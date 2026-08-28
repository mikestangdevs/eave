#!/usr/bin/env bash
# Slice P3 — mutation testing for the new assertions.
#
# A test that cannot fail is a claim, not a check. Each mutation below
# breaks exactly one thing this slice asserts; the run is a PASS only if
# the NAMED test fails. Every mutation is reverted before the next.
set -uo pipefail
cd "$(dirname "$0")/.."

UI=crates/eave-app/src/ui.rs
ICONS=crates/eave-app/src/icons.rs
ADDED=crates/eave-app/assets/icons/status-added.svg
PLUS=crates/eave-app/assets/icons/plus.svg

run_test() { CARGO_TARGET_DIR=target cargo test -p eave-app "$1" 2>&1 | grep -E "^test result"; }

check() { # name, expected-failing-test
  local label="$1" filter="$2"
  local out; out="$(run_test "$filter")"
  if echo "$out" | grep -q "FAILED\|0 passed"; then
    echo "CAUGHT   $label  (by $filter)"
  else
    echo "ESCAPED  $label  (>>> $filter still passes: $out)"
  fi
}

cp "$UI" /tmp/p3.ui.bak; cp "$ADDED" /tmp/p3.added.bak; cp "$PLUS" /tmp/p3.plus.bak

echo "── M1: point the rail toggle at the send button's box ──"
python3 - <<'PY'
import io
p='crates/eave-app/src/ui.rs'; s=io.open(p,encoding='utf-8').read()
i=s.index('icons::PANEL_LEFT,')
j=s.index('theme::ICON_TOGGLE,', i)
s=s[:j]+'theme::ICON_SEND,'+s[j+len('theme::ICON_TOGGLE,'):]
io.open(p,'w',encoding='utf-8').write(s)
PY
check "rail toggle sized from the wrong token" every_swapped_control_draws_its_glyph_in_the_box_its_token_names
cp /tmp/p3.ui.bak "$UI"

echo "── M2: strip the viewBox from one drawing ──"
sed -i '' 's/ viewBox="0 0 24 24"//' "$PLUS"
check "an icon with no viewBox" every_icon_parses_with_the_renderers_own_parser
cp /tmp/p3.plus.bak "$PLUS"

echo "── M3: thin one stroke without updating the weight table ──"
sed -i '' 's/stroke-width="2.25"/stroke-width="1"/' "$ADDED"
check "a glyph redrawn at the wrong weight" effective_stroke_weight_is_uniform_across_the_set
cp /tmp/p3.added.bak "$ADDED"

echo "── M4: let one status draw a wider glyph than the others ──"
python3 - <<'PY'
import io
p='crates/eave-app/src/ui.rs'; s=io.open(p,encoding='utf-8').read()
o="""                        status_icon(f.status),
                        theme::ICON_STATUS,
                        theme::text_tertiary(),"""
n="""                        status_icon(f.status),
                        if f.status == FileStatus::Added { theme::ICON_STATUS + 2.0 } else { theme::ICON_STATUS },
                        theme::text_tertiary(),"""
assert s.count(o)==1
io.open(p,'w',encoding='utf-8').write(s.replace(o,n))
PY
check "one status wider than the rest" the_status_glyph_never_moves_the_path_beside_it
cp /tmp/p3.ui.bak "$UI"

echo "── M5: break the toggle's hitbox by hanging the glyph outside it ──"
python3 - <<'PY'
import io
p='crates/eave-app/src/ui.rs'; s=io.open(p,encoding='utf-8').read()
# Remove the rail toggle's click listener: the glyph still renders, the
# control no longer acts. This is the failure a swap can silently cause.
o="""                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sessions.update(cx, |s, cx| {
                            s.collapsed = !s.collapsed;
                            cx.notify();
                        });
                    }))"""
assert s.count(o)==1
io.open(p,'w',encoding='utf-8').write(s.replace(o,"",1))
PY
check "the toggle renders but no longer toggles" the_collapse_toggles_still_toggle_after_the_glyph_swap
cp /tmp/p3.ui.bak "$UI"

echo "── restored; confirming the tree is green again ──"
run_test icons::
run_test glyph
