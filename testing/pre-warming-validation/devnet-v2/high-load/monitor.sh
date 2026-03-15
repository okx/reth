#!/bin/bash
# Monitor high load benchmark progress
# Run this in a separate terminal while benchmark is running

while true; do
    clear
    echo "=== HIGH LOAD BENCHMARK MONITOR - $(date) ==="
    echo ""

    # Check processes
    NODE_RUNNING=$(pgrep -f "op-reth" | wc -l | tr -d ' ')
    PYTHON_RUNNING=$(pgrep -f "python3" | wc -l | tr -d ' ')
    echo "Processes: Node=$NODE_RUNNING | Python senders=$PYTHON_RUNNING"
    echo ""

    # Find latest results dir
    DIR=$(ls -td /Users/lakshmikanth/Documents/optimisation/reth/.high-load-benchmark-* 2>/dev/null | head -1)
    if [ -n "$DIR" ]; then
        echo "Results Dir: $(basename $DIR)"
        echo ""

        # Check for results files
        if [ -f "$DIR/results_off.json" ]; then
            echo "Phase 1 (OFF): COMPLETED"
            TPS=$(grep '"tps"' "$DIR/results_off.json" | grep -oE '[0-9.]+')
            HIT=$(grep '"cache_hit_rate"' "$DIR/results_off.json" | grep -oE '[0-9.]+')
            echo "  TPS: $TPS | Cache Hit: $HIT%"
        else
            echo "Phase 1 (OFF): IN PROGRESS"
        fi

        if [ -f "$DIR/results_on.json" ]; then
            echo "Phase 2 (ON): COMPLETED"
            TPS=$(grep '"tps"' "$DIR/results_on.json" | grep -oE '[0-9.]+')
            HIT=$(grep '"cache_hit_rate"' "$DIR/results_on.json" | grep -oE '[0-9.]+')
            echo "  TPS: $TPS | Cache Hit: $HIT%"
        fi
        echo ""

        # Check sender progress for current phase
        PHASE="OFF"
        if [ -f "$DIR/results_off.json" ]; then
            PHASE="ON"
        fi

        echo "=== ${PHASE} Phase Sender Progress ==="
        TOTAL_SUCCESS=0
        TOTAL_FAILED=0
        for i in 0 1 2 3 4 5 6 7 8 9; do
            OUT="$DIR/${PHASE}_sender_${i}.out"
            if [ -f "$OUT" ]; then
                LAST=$(grep "PROGRESS:" "$OUT" 2>/dev/null | tail -1)
                FINAL=$(grep "^SENDER_" "$OUT" 2>/dev/null | tail -1)
                if [ -n "$FINAL" ]; then
                    SUCC=$(echo "$FINAL" | grep -oE 'SUCCESS:[0-9]+' | cut -d: -f2)
                    FAIL=$(echo "$FINAL" | grep -oE 'FAILED:[0-9]+' | cut -d: -f2)
                    TOTAL_SUCCESS=$((TOTAL_SUCCESS + SUCC))
                    TOTAL_FAILED=$((TOTAL_FAILED + FAIL))
                    echo "  Sender $i: DONE (success=$SUCC, failed=$FAIL)"
                elif [ -n "$LAST" ]; then
                    PCT=$(echo "$LAST" | cut -d: -f3)
                    SUCC=$(echo "$LAST" | cut -d: -f4)
                    FAIL=$(echo "$LAST" | cut -d: -f5)
                    TOTAL_SUCCESS=$((TOTAL_SUCCESS + SUCC))
                    TOTAL_FAILED=$((TOTAL_FAILED + FAIL))
                    echo "  Sender $i: $PCT (success=$SUCC, failed=$FAIL)"
                fi
            fi
        done
        echo ""
        echo "Total: success=$TOTAL_SUCCESS failed=$TOTAL_FAILED"

        # Show latest snapshot
        SNAP_FILE="$DIR/${PHASE}_snapshots.log"
        if [ -f "$SNAP_FILE" ]; then
            LAST_SNAP=$(tail -1 "$SNAP_FILE")
            HITS=$(echo "$LAST_SNAP" | cut -d'|' -f3)
            MISSES=$(echo "$LAST_SNAP" | cut -d'|' -f4)
            BLOCK=$(echo "$LAST_SNAP" | cut -d'|' -f7)
            echo ""
            echo "Latest Metrics: Hits=$HITS Misses=$MISSES Block=$BLOCK"
        fi
    else
        echo "No benchmark running yet..."
    fi

    echo ""
    echo "Press Ctrl+C to stop monitoring"
    sleep 5
done

