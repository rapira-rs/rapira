<?php
if (($_GET['code'] ?? '') === '404') {
	http_response_code(404);
}
echo "ok";
