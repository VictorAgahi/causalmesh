import { Controller, Get, Post } from '@nestjs/common';

@Controller('invoices')
export class InvoicesController {
  @Get(':invoiceId')
  getInvoice() {
    return 'invoice';
  }

  @Post(':invoiceId/pay')
  payInvoice() {
    return 'paid';
  }
}
