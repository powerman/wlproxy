# TODO

## Phase 1: Fix found problems

- [ ] 1. Validate message_size >= 8 in read_packet (prevent underflow)
- [ ] 2. Fix write_arg_uint error message (says "string length", should be "arg")
- [ ] 3. Remove underscore from `_xdgwmbase_type_id` parameter (it IS used)
- [ ] 4. Clear `cache_reg_id` on registry delete

## Phase 2: Add missing tests

- [ ] 5. Add test: FD forwarding client→server
- [ ] 6. Add test: multiple concurrent connections
- [ ] 7. Add test: block + prefix title combined
- [ ] 8. Add test: validate_interfaces unknown name warning
- [ ] 9. Add test: empty app_id/title replacement
- [ ] 10. Add test: block with FD in bind request (client→server)

## Phase 3: Best practices improvements

- [ ] 11. Replace `&'static str` errors with `thiserror` in proto.rs

## Phase 4: Documentation (after user approval)

- [ ] 12. Document lock-file mechanism

## Phase 5: Clarifications

- [ ] 13. Explain SOCK_SEQPACKET support
