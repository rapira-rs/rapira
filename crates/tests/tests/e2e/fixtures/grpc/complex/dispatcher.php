<?php

use E2e\Grpc\InspectReply;
use E2e\Grpc\InspectRequest;
use Rapira\Exception\ClosedException;

spl_autoload_register(static function (string $class): void {
    $path = __DIR__ . '/generated/' . str_replace('\\', '/', $class) . '.php';
    if (is_file($path)) {
        require $path;
    }
});

$dispatcher = \Rapira\get_dispatcher();

try {
    while (true) {
        $call = $dispatcher->receive();
        $request = new InspectRequest();
        $request->mergeFromString($call->getMessage());
        $order = $request->getOrder();
        $skus = [];
        $quantity = 0;
        foreach ($order->getItems() as $item) {
            $skus[] = $item->getSku();
            $quantity += $item->getQuantity();
        }

        $reply = new InspectReply([
            'order_id' => $order->getId(),
            'customer_name' => $order->getCustomer()->getName(),
            'customer_city' => $order->getCustomer()->getAddress()->getCity(),
            'skus' => $skus,
            'total_quantity' => $quantity,
            'priority' => $order->getLabels()['priority'],
            'placed_at' => $order->getPlacedAt(),
            'note' => $request->getNote()->unpack()->getText(),
            'gift' => $request->getOptions()->getFields()['gift']->getBoolValue(),
            'delivery' => match ($order->getDelivery()) {
                'ship_to' => 'ship_to:' . $order->getShipTo()->getCity(),
                'pickup_point' => 'pickup_point:' . $order->getPickupPoint(),
            },
            'token' => $order->getToken(),
        ]);
        $call->getResponseMetadata()->addHeader('x-worker', (string) getmypid());
        $call->respond($reply->serializeToString());
    }
} catch (ClosedException) {
}
