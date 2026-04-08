#!/bin/bash
# Test script to verify session switching doesn't lose terminal content
# This would require manual testing in an actual terminal, but documents the test case

echo "Test case for session switching robustness:"
echo ""
echo "1. Start mbulet (./target/debug/mbulet)"
echo "2. In sidebar (should be focused by default):"
echo "   - Press 'n' to create a new session"
echo "   - Type some text in the terminal: echo 'Test content in session 2'"
echo "3. Press Ctrl+B then Tab to go back to sidebar"
echo "4. Press 'k' to switch to session 1"
echo "5. Press 'j' to switch back to session 2"
echo "6. Repeat steps 4-5 multiple times (10+)"
echo ""
echo "Expected: Terminal content remains visible after each switch"
echo "Previous bug: Content would disappear after multiple switches"
echo ""
echo "The fix ensures:"
echo "  - Parser dimensions match actual render area (relative sizing)"
echo "  - Parser is only reset when dimensions change (preserves state)"
echo "  - No desync between calculated and allocated terminal space"
