#!/bin/bash
# Test script for session move functionality

set -e

echo "=== Testing Session Move Functionality ==="
echo ""

# Stop any existing daemon
pkill -f "mbulet.*daemon" 2>/dev/null || true
sleep 0.5

SOCKET="/tmp/mbulet-test-move.sock"
rm -f "$SOCKET"

# Start daemon in background
echo "1. Starting daemon..."
./target/release/mbulet daemon "$SOCKET" &
DAEMON_PID=$!
sleep 1

# Create test sessions
echo "2. Creating test sessions..."
for i in {1..3}; do
    echo "   Creating session-$i"
    echo '{"NewSession":{"name":"session-'$i'","cols":80,"rows":24,"startup_cmds":[]}}' | \
        nc -U "$SOCKET" > /dev/null 2>&1 || echo "(nc failed, continuing)"
    sleep 0.3
done

# List sessions
echo ""
echo "3. Initial session list:"
echo '{"ListSessions":null}' | nc -U "$SOCKET" 2>/dev/null | jq -r '.SessionList.sessions[] | "\(.id): \(.name)"' || echo "(failed to list)"

# Test moving session (simulate what client does)
echo ""
echo "4. Testing ReorderSession:"
echo "   Moving session with id=3 to index 0..."
# Note: This is a raw protocol test. In the real UI, you'd press Ctrl+B then Up/Down
echo '{"ReorderSession":{"id":3,"new_index":0}}' | nc -U "$SOCKET" > /dev/null 2>&1 || echo "(reorder failed)"
sleep 0.3

echo "   Fetching new order..."
echo '{"ListSessions":null}' | nc -U "$SOCKET" 2>/dev/null | jq -r '.SessionList.sessions[] | "\(.id): \(.name)"' || echo "(failed to list)"

# Cleanup
echo ""
echo "5. Cleaning up..."
echo '{"Shutdown":null}' | nc -U "$SOCKET" > /dev/null 2>&1 || true
sleep 0.5
kill $DAEMON_PID 2>/dev/null || true
rm -f "$SOCKET"

echo ""
echo "=== Test complete ==="
echo ""
echo "To test interactively:"
echo "1. Start daemon:  ./target/release/mbulet daemon /tmp/mbulet.sock"
echo "2. Start client:  ./target/release/mbulet /tmp/mbulet.sock"
echo "3. In sidebar:    Press Ctrl+B, then Up or Down to move sessions"
echo "4. Change prefix: Edit PREFIX_KEY constant in src/client.rs"
