#!/bin/bash
S="/home/jason/.grok/sessions/%2Fhome%2Fjason/019f7801-7da9-7b91-8a63-652371e1b68b"
m=no; [ -f "$S/mission.json" ] && m=yes
ideas=$(grep -c '"kind":"idea"' "$S/blackboard.jsonl" 2>/dev/null || echo 0)
compass=$(grep -c 'compass' "$S/chat_history.jsonl" 2>/dev/null || echo 0)
waiting=$(grep -c '"waiting_on"' "$S/chat_history.jsonl" 2>/dev/null || echo 0)
goalcap=$(grep -c 'goal mode' "$S/chat_history.jsonl" 2>/dev/null || echo 0)
calls=$(grep -c '"name":"' "$S/chat_history.jsonl" 2>/dev/null || echo 0)
stuck=$(grep -c 'Stuck signal' "$S/chat_history.jsonl" 2>/dev/null || echo 0)
incub=$(ls "$S/subagents" 2>/dev/null | wc -l)
boardn=$(wc -l < "$S/blackboard.jsonl" 2>/dev/null || echo 0)
report=$(ls /home/jason/rhythm-test/ 2>/dev/null | tr '\n' ',' )
echo "map:$m ideas:$ideas compass:$compass waiting:$waiting cap:$goalcap calls:$calls stuck:$stuck subagents:$incub board:$boardn files:[$report]"
