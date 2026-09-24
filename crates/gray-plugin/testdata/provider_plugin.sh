#!/bin/sh
# Protocol 1.2 provider RPC fixture. Test values only; never real credentials.
while IFS= read -r line; do
  case "$line" in
    *plugin/manifest*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{"id":%s,"result":{"name":"provider-fixture","version":"0.1.0","protocol":"1.2","capabilities":["provider.credentials"],"tools":[]}}\n' "$id"
      ;;
    *provider/auth/start*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{"id":%s,"result":{"operation_id":"op-test","status":"pending","verification_uri":"https://auth.example.test/verify?state=state-test","expires_at":4102444800,"retry_after_ms":10}}\n' "$id"
      ;;
    *provider/auth/poll*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      case "$line" in
        *'"operation_id":"bad"'*)
          printf '{"id":%s,"result":{"state":"mystery"}}\n' "$id"
          ;;
        *)
          printf '{"id":%s,"result":{"state":"completed","credential":{"secrets":{"access_token":"test-access"},"metadata":{"account_id":"acct_test"},"expires_at":4102444800}}}\n' "$id"
          ;;
      esac
      ;;
    *provider/auth/cancel*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{"id":%s,"result":{}}\n' "$id"
      ;;
    *provider/auth/refresh*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{"id":%s,"result":{"secrets":{"access_token":"test-access","refresh_token":"test-refresh"},"metadata":{"account_id":"acct_test"},"expires_at":4102444800}}\n' "$id"
      ;;
    *provider/auth/revoke*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{"id":%s,"result":{"status":"revoked"}}\n' "$id"
      ;;
    *provider/models*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{"id":%s,"result":{"models":[{"id":"gpt-test","name":"GPT Test","context_window":128000,"reasoning_efforts":["off","low"]}]}}\n' "$id"
      ;;
    *plugin/shutdown*)
      exit 0
      ;;
  esac
done
