import { Controller } from '@nestjs/common';
import { Money } from './money';

@Controller('checkout')
export class CheckoutController {
  @GrpcMethod('CheckoutService', 'PlaceOrder')
  placeOrder(data: any): Money {
    return new Money();
  }
}
