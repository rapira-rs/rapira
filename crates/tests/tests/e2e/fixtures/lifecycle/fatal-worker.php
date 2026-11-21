<?php
// An uncaught exception before the request loop. Every boot fails, so each worker of the gen-0 pool becomes unhealthy and the master exits with code 70.
throw new RuntimeException('fatal-worker: intentional bootstrap failure');
