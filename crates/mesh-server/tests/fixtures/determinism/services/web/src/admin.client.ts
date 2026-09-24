export class AdminClient {
  private adminService: any;

  constructor(private readonly client: any) {}

  onModuleInit() {
    this.adminService = this.client.getService('AdminService');
  }
}
