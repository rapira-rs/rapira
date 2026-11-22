#include "rapira_sapi.h"
#include <Zend/zend_extensions.h>

// OPcache clears its startup hook if it cannot start.
// https://github.com/php/php-src/blob/php-8.6.0/ext/opcache/ZendAccelerator.c#L3073-L3102
bool rapira_opcache_enabled(void) {
    const zend_extension *opcache = zend_get_extension("Zend OPcache");
    if (!opcache || !opcache->startup) {
        return false;
    }
    zend_string *enabled = zend_ini_str(ZEND_STRL("opcache.enable"), false);
    return enabled && zend_ini_parse_bool(enabled);
}
