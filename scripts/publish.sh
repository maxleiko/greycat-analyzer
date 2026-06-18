#!/bin/bash
sha256_hash=$(echo -n "$GET_GC_CI_PASS" | openssl dgst -sha256 | cut -d ' ' -f2)
base64url_token=$(echo -n "root:$sha256_hash" | base64 -w 0 )
token=$(curl -s -d "[\"${base64url_token}\", false]" -X POST https://get.greycat.io/runtime::User::login | tr -d '"')
package="lang"

cd dist || exit

find . -type f -name '*.zip' -print0 | while IFS= read -r -d '' file; do
    short_file=$(echo "$file" | sed 's/^.//'| sed 's/^.//')
    curl -s -X PUT -H "Authorization: $token" -T $file https://get.greycat.io/files/$package/$short_file
done

curl -s -X PUT -H "Authorization: $token" -d "${PROJECT_VERSION_MAJOR_MINOR}/${PROJECT_VERSION_SIMPLE}" -H "Content-Type: text/plain" "https://get.greycat.io/files/$package/${CI_COMMIT_REF_NAME}/latest"
